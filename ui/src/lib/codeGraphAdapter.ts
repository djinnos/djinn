/**
 * codeGraphAdapter — translate the `code_graph snapshot` MCP response into
 * a graphology graph ready for Sigma + ForceAtlas2 (PR D2).
 *
 * Per-type ForceAtlas2 mass is the trick that makes a force-directed
 * layout *feel* hierarchical: heavy folder/project nodes pull their
 * file/symbol children into clusters instead of mashing everything
 * into the center.  Plan §PR-D2 mass table:
 *
 *   Project = 50
 *   Folder  = 15
 *   File    = 3
 *   Symbol  (Class/Function/Method/...) = 2
 *
 * The snapshot wire shape is pinned by the inter-PR contract
 * (`code_graph snapshot` section in the plan).  We re-declare the
 * relevant subset here as plain interfaces so the parser layer
 * doesn't depend on MCP autogen — the autogen treats the response as
 * `Record<string, any>` because `CodeGraphResponse` is `#[serde(untagged)]`.
 */

import Graph from "graphology";

export type SnapshotNodeKind = "file" | "folder" | "symbol";

/** One node entry on the wire (matches `bridge::SnapshotNode`). */
export interface SnapshotNode {
  /** RepoNodeKey: `"file:..."` or `"symbol:..."`. */
  id: string;
  kind: SnapshotNodeKind;
  label: string;
  /**
   * SCIP symbol-kind (lowercased): `"function"`, `"class"`, `"method"`, ….
   * Absent for file / folder nodes.
   */
  symbol_kind?: string;
  /** Repo-relative file path; present for `kind="symbol"` nodes. */
  file_path?: string;
  pagerank: number;
  /** Populated post-F3 (Leiden community detection); absent in D2. */
  community_id?: string;
}

/** One edge entry on the wire (matches `bridge::SnapshotEdge`). */
export interface SnapshotEdge {
  from: string;
  to: string;
  /** `RepoGraphEdgeKind` Debug variant name (e.g. `"SymbolReference"`). */
  kind: string;
  confidence: number;
  reason?: string;
}

/** Full snapshot payload — matches `bridge::SnapshotPayload`. */
export interface SnapshotPayload {
  project_id: string;
  git_head: string;
  generated_at: string;
  truncated: boolean;
  total_nodes: number;
  total_edges: number;
  node_cap: number;
  nodes: SnapshotNode[];
  edges: SnapshotEdge[];
}

/**
 * Top-level wire envelope for the snapshot op.  The `code_graph`
 * MCP enum is `#[serde(untagged)]`, so the discriminator is the
 * `snapshot` field name (per the inter-PR contract).
 */
export interface SnapshotResponse {
  snapshot: SnapshotPayload;
  next_step?: string | null;
}

/** Best-effort runtime narrowing for the untagged `code_graph` response. */
export function parseSnapshotResponse(value: unknown): SnapshotPayload | null {
  if (!isRecord(value)) return null;
  const inner = (value as Record<string, unknown>).snapshot;
  if (!isRecord(inner)) return null;
  const nodes = Array.isArray(inner.nodes)
    ? (inner.nodes.filter(isRecord) as Array<Record<string, unknown>>)
    : [];
  const edges = Array.isArray(inner.edges)
    ? (inner.edges.filter(isRecord) as Array<Record<string, unknown>>)
    : [];
  return {
    project_id: String(inner.project_id ?? ""),
    git_head: String(inner.git_head ?? ""),
    generated_at: String(inner.generated_at ?? ""),
    truncated: Boolean(inner.truncated),
    total_nodes: Number(inner.total_nodes ?? nodes.length),
    total_edges: Number(inner.total_edges ?? edges.length),
    node_cap: Number(inner.node_cap ?? nodes.length),
    nodes: nodes
      .map((n) => ({
        id: String(n.id ?? ""),
        kind: normalizeKind(n.kind),
        label: String(n.label ?? ""),
        symbol_kind:
          typeof n.symbol_kind === "string" ? n.symbol_kind : undefined,
        file_path: typeof n.file_path === "string" ? n.file_path : undefined,
        pagerank: Number(n.pagerank ?? 0),
        community_id:
          typeof n.community_id === "string" ? n.community_id : undefined,
      }))
      .filter((n) => n.id.length > 0),
    edges: edges
      .map((e) => ({
        from: String(e.from ?? ""),
        to: String(e.to ?? ""),
        kind: String(e.kind ?? ""),
        confidence: Number(e.confidence ?? 0),
        reason: typeof e.reason === "string" ? e.reason : undefined,
      }))
      .filter((e) => e.from.length > 0 && e.to.length > 0),
  };
}

function isRecord(value: unknown): value is Record<string, unknown> {
  return typeof value === "object" && value !== null && !Array.isArray(value);
}

function normalizeKind(value: unknown): SnapshotNodeKind {
  if (value === "folder" || value === "file" || value === "symbol") return value;
  return "symbol";
}

// ── Mass table (plan §PR-D2) ────────────────────────────────────────────────

/**
 * Per-type ForceAtlas2 mass.  Heavy nodes act as gravity wells, pulling
 * lighter neighbors into a hierarchical-feeling layout without an
 * explicit hierarchical algorithm.
 */
export const NODE_MASS: Record<string, number> = {
  project: 50,
  folder: 15,
  file: 3,
  symbol: 2,
  // Symbol-kind overrides — top-level symbols still default to 2, but
  // heavier "type-like" symbols anchor their methods.
  class: 2,
  struct: 2,
  interface: 2,
  enum: 2,
  function: 2,
  method: 2,
  constructor: 2,
};

export function massForNode(node: SnapshotNode): number {
  // Symbol-kind override (e.g. `class` vs generic `symbol`) when the
  // SCIP indexer populated `symbol_kind`.
  if (node.kind === "symbol" && node.symbol_kind) {
    const fromKind = NODE_MASS[node.symbol_kind];
    if (typeof fromKind === "number") return fromKind;
  }
  const fromTopLevel = NODE_MASS[node.kind];
  if (typeof fromTopLevel === "number") return fromTopLevel;
  return 1;
}

// ── Color palette ───────────────────────────────────────────────────────────

/** Tailwind-aligned colors so the canvas reads against the shadcn shell. */
export const NODE_COLORS = {
  file: "#60a5fa", // blue-400
  folder: "#a78bfa", // violet-400
  project: "#f472b6", // pink-400
  symbol_class: "#fbbf24", // amber-400
  symbol_struct: "#fbbf24",
  symbol_interface: "#34d399", // emerald-400
  symbol_enum: "#fbbf24",
  symbol_function: "#a3e635", // lime-400
  symbol_method: "#a3e635",
  symbol_constructor: "#a3e635",
  symbol_default: "#cbd5e1", // slate-300
  edge_default: "rgba(148, 163, 184, 0.55)", // slate-400 @ 55%
} as const;

export function colorForNode(node: SnapshotNode): string {
  if (node.kind === "file") return NODE_COLORS.file;
  if (node.kind === "folder") return NODE_COLORS.folder;
  if (node.kind === "symbol") {
    switch (node.symbol_kind) {
      case "class":
        return NODE_COLORS.symbol_class;
      case "struct":
        return NODE_COLORS.symbol_struct;
      case "interface":
        return NODE_COLORS.symbol_interface;
      case "enum":
        return NODE_COLORS.symbol_enum;
      case "function":
        return NODE_COLORS.symbol_function;
      case "method":
        return NODE_COLORS.symbol_method;
      case "constructor":
        return NODE_COLORS.symbol_constructor;
      default:
        return NODE_COLORS.symbol_default;
    }
  }
  return NODE_COLORS.symbol_default;
}

// ── Adapter ─────────────────────────────────────────────────────────────────

export interface BuildGraphOptions {
  /**
   * Drop self-loops?  Sigma renders self-loops as a small bow on the
   * node, which is rarely meaningful in a code graph and clutters the
   * rendering.  Default `true`.
   */
  dropSelfLoops?: boolean;
}

/**
 * Convert a snapshot payload into a graphology `Graph` configured for
 * Sigma + ForceAtlas2.  Each node carries `mass` (consumed by FA2 via
 * the `nodeMassReducer` we wire up in `useSigmaGraph`), `size`
 * (visual; pagerank-scaled), `color`, and `label`.
 *
 * Initial node positions are pseudo-randomized in `[-1, 1]^2` — FA2
 * does the heavy lifting.  We *don't* warm with circular / random
 * layouts because the per-type mass already produces a nice spread
 * once FA2 starts iterating.
 */
export function buildGraphFromSnapshot(
  snapshot: SnapshotPayload,
  options: BuildGraphOptions = {},
): Graph {
  const dropSelfLoops = options.dropSelfLoops ?? true;
  const graph = new Graph({ multi: true, type: "directed" });

  // Pagerank-scaled visual size.  Cap at [3, 18] so the largest hub
  // doesn't drown the canvas and the smallest leaf is still clickable.
  const ranks = snapshot.nodes.map((n) => n.pagerank);
  const maxRank = ranks.length > 0 ? Math.max(...ranks, 0.000_001) : 1;

  for (const node of snapshot.nodes) {
    if (graph.hasNode(node.id)) continue;
    const normalized = node.pagerank / maxRank;
    const size = 3 + normalized * 15;
    graph.addNode(node.id, {
      // Sigma reads `label`, `x`, `y`, `size`, `color`. The rest is
      // free-form metadata reducers / interaction layers can read.
      label: node.label,
      x: (Math.random() - 0.5) * 2,
      y: (Math.random() - 0.5) * 2,
      size,
      color: colorForNode(node),
      // Custom fields (used by FA2 mass reducer + D3 highlight layer):
      mass: massForNode(node),
      kind: node.kind,
      symbolKind: node.symbol_kind,
      pagerank: node.pagerank,
      filePath: node.file_path,
      communityId: node.community_id,
    });
  }

  for (const edge of snapshot.edges) {
    if (!graph.hasNode(edge.from) || !graph.hasNode(edge.to)) continue;
    if (dropSelfLoops && edge.from === edge.to) continue;
    graph.addEdge(edge.from, edge.to, {
      kind: edge.kind,
      confidence: edge.confidence,
      reason: edge.reason,
      // Edge-confidence drives visual weight: high-confidence edges
      // are slightly thicker so the eye trails them.
      size: 0.4 + edge.confidence * 1.1,
      color: NODE_COLORS.edge_default,
    });
  }

  return graph;
}
