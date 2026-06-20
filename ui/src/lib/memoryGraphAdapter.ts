/**
 * memoryGraphAdapter — translate the `memory_graph` MCP response into a
 * graphology graph ready for Sigma + ForceAtlas2, then run Louvain community
 * detection over the wikilink graph and stamp every clustered note with
 * `communityId` / `communityLabel` attributes.
 *
 * This is the memory-graph analogue of `codeGraphAdapter.buildGraphFromSnapshot`:
 * it produces a Sigma-renderable graphology graph and writes the community
 * metadata that reducers (dim/emphasize per community on hover) and the
 * per-community attraction pass read.
 *
 * Pure functions only — no React, no fetch, no Sigma, no store. The canvas
 * component (`MemoryGraphCanvas`) wires those in. `buildMemoryGraph` and
 * `parseMemoryGraphResponse` are the stable export surface consumed by the
 * canvas (task 4).
 *
 * Scope notes:
 *   - The graph is undirected (wikilinks have no meaningful direction for
 *     layout) and unweighted. The MCP payload ships directed
 *     `source_id`/`target_id` edges, but ForceAtlas2 + community detection
 *     treat them symmetrically here.
 *   - Singleton communities / notes with no intra-cluster edges collapse
 *     into the "unclustered" bucket: `clusterMemoryCommunities` already
 *     excludes them, and `applyPerCommunityAttraction` skips nodes without
 *     a non-empty `communityId`. Those notes remain in the normal FA2 flow
 *     and are never attraction-eligible.
 *   - `kind: "co_access"` edges are added by the canvas (task 4), not by
 *     this adapter — the adapter only sees wikilink edges from the
 *     `memory_graph` response.
 */

import Graph from "graphology";
import type { MemoryGraphOutput } from "@/api/generated/mcp-tools.gen";
import {
  clusterMemoryCommunities,
  type MemoryCommunityMetadata,
} from "@/lib/memoryCommunityClustering";

/** Node attribute name carrying the stable community id (16-hex sha256). */
export const MEMORY_COMMUNITY_ID_ATTRIBUTE = "communityId";
/** Node attribute name carrying the top-K community label (space-joined terms). */
export const MEMORY_COMMUNITY_LABEL_ATTRIBUTE = "communityLabel";

// ── Types: graphology attribute shapes (task 4 stable surface) ──────────────

/**
 * Per-node attributes emitted by `buildMemoryGraph`. The color is a
 * deterministic mapping from `note_type` (with an `is_orphan` override),
 * size is scaled from `connection_count` with a floor and a cap, and
 * `x`/`y` are seeded so the same input + seed always produces the same
 * layout.
 */
export interface MemoryGraphNodeAttrs {
  label: string;
  color: string;
  size: number;
  note_type: string;
  is_orphan: boolean;
  broken_targets: string[];
  permalink: string;
  x: number;
  y: number;
  // ── Pass-through attrs read by community clustering & reducers ────────────
  /** Graphology expects a `label`-ish display name on every node. */
  title: string;
  noteType: string;
  folder: string;
  connectionCount: number;
}

/** Edge relationship kinds the adapter can emit. */
export type TypedEdgeKind = "builds_on" | "contradicts" | "supersedes" | "exemplifies" | "derived_from";
export type MemoryGraphEdgeKind = "wikilink" | "broken" | "co_access" | TypedEdgeKind;

/**
 * Per-edge attributes. `kind: "wikilink"` is the default for resolved
 * edges; `kind: "broken"` marks a stub edge produced from a node's
 * `broken_targets` entry (the target note does not exist). The canvas
 * adds `kind: "co_access"` on top — never emitted here.
 */
export interface MemoryGraphEdgeAttrs {
  color: string;
  size: number;
  kind: MemoryGraphEdgeKind;
  /** Present on `broken` stub edges (the raw wikilink text) and wikilink edges. */
  raw_text?: string;
  /** Dashed line flag (e.g. for `contradicts` typed edges). */
  dashed?: boolean;
  /** Weight from the server (present on typed edges). */
  weight?: number;
}

// ── Color palette ───────────────────────────────────────────────────────────

/**
 * Deterministic `note_type → color` palette. Each known type maps to a
 * Tailwind-400 hue that reads on the near-black canvas. Unknown types fall
 * back to `DEFAULT_COLOR`. Orphan nodes are overridden to `ORPHAN_OVERRIDE`
 * regardless of their type.
 *
 * The enrichment-created note types (`entity`, `claim`) — see epic diei /
 * task qp5s — get distinct hues so they read as a separate class of node:
 *   - `entity`: teal-400 — recurring systems / concepts surfaced by the
 *     enrichment pass.
 *   - `claim`: pink-400 — the decisions the memory records.
 */
export const PALETTE: Readonly<Record<string, string>> = Object.freeze({
  adr: "#a78bfa", // violet-400
  pattern: "#60a5fa", // blue-400
  case: "#34d399", // emerald-400
  pitfall: "#f87171", // red-400
  research: "#fbbf24", // amber-400
  reference: "#94a3b8", // slate-400
  entity: "#2dd4bf", // teal-400 — enrichment-created recurring system
  claim: "#f472b6", // pink-400 — enrichment-created decision
});

/** Fallback color for nodes whose `note_type` is not in the palette. */
export const DEFAULT_COLOR = "#cbd5e1"; // slate-300

/**
 * Orphan override — a node with `is_orphan: true` is always red regardless
 * of its `note_type`. This is the visual flag the epic calls for.
 */
export const ORPHAN_OVERRIDE = "#ef4444"; // red-500

/**
 * Resolve the display color for a node. Orphan nodes always win (red);
 * otherwise the palette maps `note_type` to a hue, with `DEFAULT_COLOR` as
 * the fallback for unknown types.
 */
export function colorForNote(noteType: string, isOrphan: boolean): string {
  if (isOrphan) return ORPHAN_OVERRIDE;
  return PALETTE[noteType] ?? DEFAULT_COLOR;
}

// ── Size scaling ────────────────────────────────────────────────────────────

/** Minimum node size — a node with 0 connections is still a clickable target. */
export const MIN_NODE_SIZE = 4;
/** Maximum node size — caps hub nodes so they don't swallow the canvas. */
export const MAX_NODE_SIZE = 20;
/** Scale factor applied to the sqrt of `connection_count`. */
const SIZE_SCALE = 2;

/**
 * Scale `connection_count` into a bounded visual size:
 * `clamp(MIN_NODE_SIZE, MAX_NODE_SIZE, MIN_NODE_SIZE + sqrt(count) * SCALE)`.
 *
 * A node with 0 connections sits at the floor (`MIN_NODE_SIZE`), and a hub
 * with 100+ connections is capped at `MAX_NODE_SIZE`.
 */
export function sizeForConnectionCount(connectionCount: number): number {
  const count =
    typeof connectionCount === "number" && Number.isFinite(connectionCount) && connectionCount > 0
      ? connectionCount
      : 0;
  return Math.max(
    MIN_NODE_SIZE,
    Math.min(MAX_NODE_SIZE, MIN_NODE_SIZE + Math.sqrt(count) * SIZE_SCALE),
  );
}

// ── Seeded PRNG + golden-angle spiral ───────────────────────────────────────

/**
 * Small deterministic PRNG (mulberry32) so the same `(payload, seed)` pair
 * always produces identical positions. Exported for tests that want to
 * assert the exact stream.
 */
export function createSeededRandom(seed: number): () => number {
  let state = seed >>> 0;
  return () => {
    state = (state + 0x6d2b79f5) >>> 0;
    let t = state;
    t = Math.imul(t ^ (t >>> 15), t | 1);
    t ^= t + Math.imul(t ^ (t >>> 7), t | 61);
    return ((t ^ (t >>> 14)) >>> 0) / 4294967296;
  };
}

const GOLDEN_ANGLE = Math.PI * (3 - Math.sqrt(5));

/**
 * Seeded golden-angle spiral position for node `index`. The spiral spreads
 * nodes with even areal density; the seeded PRNG adds a small jitter so two
 * builds with the same seed are bit-identical but the initial layout isn't
 * a perfect lattice (FA2 converges faster from a perturbed start).
 */
function seededPosition(
  index: number,
  rng: () => number,
): { x: number; y: number } {
  const angle = index * GOLDEN_ANGLE;
  const radius = Math.sqrt(index + 1) * 6;
  const jitter = 1.5;
  return {
    x: radius * Math.cos(angle) + (rng() - 0.5) * jitter,
    y: radius * Math.sin(angle) + (rng() - 0.5) * jitter,
  };
}

// ── Edge styling ────────────────────────────────────────────────────────────

const WIKILINK_EDGE_COLOR = "#2d2d3d";
const WIKILINK_EDGE_SIZE = 0.8;
const BROKEN_EDGE_COLOR = "#f87171"; // red-400 — matches the orphan flag hue
const BROKEN_EDGE_SIZE = 1.0;

/**
 * Per-kind styling for typed association edges surfaced from `note_associations`.
 * The canvas (task 4) reads these to render distinct visual lanes.
 */
export const TYPED_EDGE_STYLES: Readonly<Record<string, { color: string; size: number; dashed: boolean }>> = Object.freeze({
  supersedes:   { color: "#22c55e", size: 2.0, dashed: false }, // green-500
  contradicts:  { color: "#ef4444", size: 2.0, dashed: true },   // red-500
  builds_on:    { color: "#3b82f6", size: 1.5, dashed: false }, // blue-500
  exemplifies:  { color: "#f59e0b", size: 1.5, dashed: false }, // amber-500
  derived_from: { color: "#8b5cf6", size: 1.5, dashed: false }, // violet-500
});

// ── Response parser (pure runtime guard) ────────────────────────────────────

function isRecord(value: unknown): value is Record<string, unknown> {
  return typeof value === "object" && value !== null && !Array.isArray(value);
}

/**
 * Parse a raw `memory_graph` MCP response into a typed `MemoryGraphOutput`,
 * or return `null` when the response is malformed / errored.
 *
 * Mirrors the defensive style of `parseSnapshotResponse` in
 * `codeGraphAdapter.ts`: returns `null` on an `error` field, missing
 * `nodes`, or wrong types, so the caller can render the empty state without
 * building an empty graphology instance.
 */
export function parseMemoryGraphResponse(raw: unknown): MemoryGraphOutput | null {
  if (!isRecord(raw)) return null;
  if (typeof raw.error === "string" && raw.error.length > 0) return null;
  if (!Array.isArray(raw.nodes)) return null;

  const nodes = raw.nodes.filter(isRecord) as Array<Record<string, unknown>>;
  const edges = Array.isArray(raw.edges)
    ? (raw.edges.filter(isRecord) as Array<Record<string, unknown>>)
    : [];

  const parsedNodes = nodes
    .map((n) => {
      const brokenTargets = Array.isArray(n.broken_targets)
        ? (n.broken_targets.filter((t): t is string => typeof t === "string"))
        : [];
      return {
        id: String(n.id ?? ""),
        permalink: typeof n.permalink === "string" ? n.permalink : "",
        title: typeof n.title === "string" ? n.title : "",
        note_type: typeof n.note_type === "string" ? n.note_type : "",
        folder: typeof n.folder === "string" ? n.folder : "",
        connection_count:
          typeof n.connection_count === "number" && Number.isFinite(n.connection_count)
            ? n.connection_count
            : 0,
        is_orphan: n.is_orphan === true,
        broken_targets: brokenTargets,
      };
    })
    .filter((n) => n.id.length > 0);

  const parsedEdges = edges
    .map((e) => ({
      source_id: String(e.source_id ?? ""),
      target_id: String(e.target_id ?? ""),
      raw_text: typeof e.raw_text === "string" ? e.raw_text : "",
    }))
    .filter((e) => e.source_id.length > 0 && e.target_id.length > 0);

  const typedEdges = Array.isArray(raw.typed_edges)
    ? (raw.typed_edges.filter(isRecord) as Array<Record<string, unknown>>)
    : [];
  const parsedTypedEdges = typedEdges
    .map((e) => ({
      source_id: String(e.source_id ?? ""),
      target_id: String(e.target_id ?? ""),
      kind: typeof e.kind === "string" ? e.kind : "",
      weight: typeof e.weight === "number" && Number.isFinite(e.weight) ? e.weight : 0,
    }))
    .filter((e) => e.source_id.length > 0 && e.target_id.length > 0 && e.kind.length > 0);

  return { nodes: parsedNodes, edges: parsedEdges, typed_edges: parsedTypedEdges };
}

// ── Core adapter ────────────────────────────────────────────────────────────

export interface BuildMemoryGraphOptions {
  /** Seed for the deterministic PRNG. Default `1`. */
  seed?: number;
}

/**
 * Build a graphology graph from a `memory_graph` payload.
 *
 * This is the pure, side-effect-free entry point the canvas (task 4) and
 * the unit tests consume. It:
 *   - Adds one node per `payload.nodes[]` with deterministic `color` (from
 *     `note_type`, orphan-overridden), bounded `size` (from
 *     `connection_count`), seeded `x`/`y`, and copy-through metadata.
 *   - Adds one `"wikilink"` edge per resolved `payload.edges[]` entry whose
 *     `source_id`/`target_id` both reference known nodes; unknown-target
 *     edges are silently dropped.
 *   - Adds one `"broken"` stub edge (self-loop on the source node) per
 *     entry in each node's `broken_targets`, carrying the raw text as the
 *     edge label. The target note does not exist as a node, so a self-edge
 *     is the simplest representation; the canvas renders it as a dashed
 *     outgoing stub.
 *
 * Determinism: the same `(payload, seed)` pair always produces an identical
 * graph (same node ids, same attributes, same edge set).
 */
export function buildMemoryGraph(
  payload: MemoryGraphOutput,
  options: BuildMemoryGraphOptions = {},
): Graph<MemoryGraphNodeAttrs, MemoryGraphEdgeAttrs> {
  const seed = options.seed ?? 1;
  const rng = createSeededRandom(seed);
  const graph = new Graph<MemoryGraphNodeAttrs, MemoryGraphEdgeAttrs>({
    type: "undirected",
    multi: true,
  });

  const knownIds = new Set<string>();

  payload.nodes.forEach((node, index) => {
    if (graph.hasNode(node.id)) return;
    const isOrphan = node.is_orphan === true;
    const brokenTargets = Array.isArray(node.broken_targets) ? node.broken_targets : [];
    const connectionCount =
      typeof node.connection_count === "number" && Number.isFinite(node.connection_count)
        ? node.connection_count
        : 0;
    const pos = seededPosition(index, rng);
    knownIds.add(node.id);
    graph.addNode(node.id, {
      label: node.title,
      color: colorForNote(node.note_type, isOrphan),
      size: sizeForConnectionCount(connectionCount),
      note_type: node.note_type,
      is_orphan: isOrphan,
      broken_targets: brokenTargets,
      permalink: node.permalink,
      x: pos.x,
      y: pos.y,
      // pass-through attrs read by community clustering & reducers
      title: node.title,
      noteType: node.note_type,
      folder: node.folder,
      connectionCount,
    });
  });

  // Resolved wikilink edges — drop any whose endpoints aren't known nodes.
  for (const edge of payload.edges) {
    if (!knownIds.has(edge.source_id) || !knownIds.has(edge.target_id)) continue;
    if (edge.source_id === edge.target_id) continue;
    graph.addEdge(edge.source_id, edge.target_id, {
      color: WIKILINK_EDGE_COLOR,
      size: WIKILINK_EDGE_SIZE,
      kind: "wikilink",
      raw_text: edge.raw_text,
    });
  }

  // Broken-target stub edges — one self-loop per entry, carrying the raw text.
  for (const node of payload.nodes) {
    const brokenTargets = Array.isArray(node.broken_targets) ? node.broken_targets : [];
    for (const raw of brokenTargets) {
      graph.addEdge(node.id, node.id, {
        color: BROKEN_EDGE_COLOR,
        size: BROKEN_EDGE_SIZE,
        kind: "broken",
        raw_text: raw,
      });
    }
  }

  // Typed association edges — only add when both endpoints are known nodes.
  const typedEdges = Array.isArray(payload.typed_edges) ? payload.typed_edges : [];
  for (const edge of typedEdges) {
    if (!knownIds.has(edge.source_id) || !knownIds.has(edge.target_id)) continue;
    if (edge.source_id === edge.target_id) continue;
    const style = TYPED_EDGE_STYLES[edge.kind] ?? { color: "#94a3b8", size: 1.0, dashed: false };
    graph.addEdge(edge.source_id, edge.target_id, {
      color: style.color,
      size: style.size,
      kind: edge.kind as MemoryGraphEdgeKind,
      dashed: style.dashed,
      weight: edge.weight,
    });
  }

  return graph;
}

// ── Community-clustering integration (ewfa epic) ────────────────────────────

/**
 * Build a graphology graph from a `memory_graph` payload WITHOUT running
 * community clustering. Used by tests that want to assert the raw topology.
 *
 * Delegates to the pure `buildMemoryGraph` so node attributes (color, size,
 * seeded positions, broken edges) are consistent with the task-4 surface.
 */
export function buildMemoryGraphFromPayload(payload: MemoryGraphOutput): Graph {
  return buildMemoryGraph(payload);
}

/**
 * Run Louvain community detection over the wikilink graph and write
 * `communityId` / `communityLabel` attributes onto clustered nodes.
 *
 * Returns the per-node community map (same shape `clusterMemoryCommunities`
 * emits) so callers can feed it straight into `applyPerCommunityAttraction`
 * without recomputing. Nodes that fall into the unclustered bucket receive
 * NO community attributes — reducers can detect absence and keep default
 * styling, and the attraction pass skips them.
 *
 * Stability contract: community ids are the first 16 hex chars of
 * `sha256(sortedMemberIds.join("\n"))`. Adding/removing a note from a
 * community therefore changes the id — documented trade-off that mirrors
 * `server/crates/djinn-graph/src/communities.rs`.
 */
export function applyMemoryCommunities(
  graph: Graph,
): Map<string, MemoryCommunityMetadata> {
  const communities = clusterMemoryCommunities(graph);
  for (const [nodeId, metadata] of communities) {
    if (!graph.hasNode(nodeId)) continue;
    graph.setNodeAttribute(
      nodeId,
      MEMORY_COMMUNITY_ID_ATTRIBUTE,
      metadata.communityId,
    );
    graph.setNodeAttribute(
      nodeId,
      MEMORY_COMMUNITY_LABEL_ATTRIBUTE,
      metadata.label,
    );
  }
  return communities;
}

/**
 * Build a community-clustered graphology graph from a `memory_graph` payload.
 *
 * This is the convenience entry point the canvas uses: it builds the topology
 * and stamps community metadata in one pass so `useSigmaGraph`'s `postLayout`
 * callback can run `applyPerCommunityAttraction` against already-decorated
 * nodes.
 */
export function buildClusteredMemoryGraph(
  payload: MemoryGraphOutput,
): { graph: Graph; communities: Map<string, MemoryCommunityMetadata> } {
  const graph = buildMemoryGraphFromPayload(payload);
  const communities = applyMemoryCommunities(graph);
  return { graph, communities };
}
