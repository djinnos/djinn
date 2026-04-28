/**
 * codeGraphAdapter — translate the `code_graph snapshot` MCP response
 * into a graphology graph ready for Sigma + ForceAtlas2.
 *
 * Three pillars produce the GitNexus-style "vivid clusters on near-black"
 * look:
 *   1. Community-driven coloring (12-hue palette indexed by hashed
 *      community_id, falling back to top-level folder when D/F3 hasn't
 *      shipped yet).
 *   2. Per-edge-kind colors / sizes / curvature so every relationship
 *      type is visually distinct rather than a uniform slate haze.
 *   3. Hierarchical seed positioning (golden-angle spiral for structural
 *      nodes, BFS jitter for files/symbols, cluster-center jitter when
 *      community ids are present) so FA2 starts close to its terminal
 *      layout instead of a chaotic random init.
 */

import Graph from "graphology";

export type SnapshotNodeKind = "file" | "folder" | "symbol";

export interface SnapshotNode {
  id: string;
  kind: SnapshotNodeKind;
  label: string;
  symbol_kind?: string;
  file_path?: string;
  pagerank: number;
  community_id?: string;
}

export interface SnapshotEdge {
  from: string;
  to: string;
  kind: string;
  confidence: number;
  reason?: string;
}

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

export interface SnapshotResponse {
  snapshot: SnapshotPayload;
  next_step?: string | null;
}

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
      .map((n) => {
        const kind = normalizeKind(n.kind);
        const rawLabel = String(n.label ?? "");
        const rawSymbolKind =
          typeof n.symbol_kind === "string" ? n.symbol_kind : null;
        return {
          id: String(n.id ?? ""),
          kind,
          label: prettifyLabel(rawLabel),
          symbol_kind:
            kind === "symbol"
              ? (rawSymbolKind ?? "other")
              : (rawSymbolKind ?? undefined),
          file_path:
            typeof n.file_path === "string" ? n.file_path : undefined,
          pagerank: Number(n.pagerank ?? 0),
          community_id:
            typeof n.community_id === "string" ? n.community_id : undefined,
        };
      })
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

/**
 * Strip SCIP descriptors down to the human-readable trailing identifier.
 *
 * The server occasionally surfaces external/cross-package symbols with
 * the raw SCIP descriptor as the label, e.g.
 *   `scip-go gomod github.com/golang/go/src . context/Context#`
 * Sigma renders these verbatim and the canvas drowns in 100-char URLs.
 *
 * SCIP grammar (best-effort): `<scheme> <manager> <pkg> <version> <descriptor>`
 * where the descriptor uses `/` as a path separator and one of the suffixes
 * `#` (type), `().` (method), `.` (term), `[]` (typeparam) on the final segment.
 *
 * We pull the last `/`-separated segment of the descriptor and strip the
 * SCIP suffix. Falls back to the original on any parse mismatch — better
 * to render something than nothing.
 */
const SCIP_LABEL_RE = /^scip-\w+\s/;

export function prettifyLabel(raw: string): string {
  if (!raw) return raw;
  if (!SCIP_LABEL_RE.test(raw)) return raw;
  const stripped = raw.replace(/`/g, "");
  const tokens = stripped.split(/\s+/);
  const descriptor = tokens[tokens.length - 1] ?? raw;
  const tail = descriptor
    .replace(/\(\)\.$/, "()")
    .replace(/[#.[\]]+$/, "");
  const segments = tail.split("/").filter((s) => s.length > 0);
  return segments.length > 0 ? segments[segments.length - 1] : raw;
}

// ── Mass scaling ────────────────────────────────────────────────────────────

/**
 * Heavy structural masses act as gravity wells in FA2 — folders blast
 * apart and pull their files with them, producing the cluster spread
 * GitNexus relies on. Scaled with node count so 12k-node monorepos
 * still spread instead of collapsing.
 */
export function massForNode(node: SnapshotNode, nodeCount: number = 0): number {
  const baseMultiplier = nodeCount > 5000 ? 2 : nodeCount > 1000 ? 1.5 : 1;

  if (node.kind === "folder") {
    if (node.symbol_kind === "project" || /project/i.test(node.label)) {
      return 50 * baseMultiplier;
    }
    return 15 * baseMultiplier;
  }
  if (node.kind === "file") return 3 * baseMultiplier;

  if (node.kind === "symbol") {
    switch (node.symbol_kind) {
      case "class":
      case "struct":
      case "interface":
      case "trait":
      case "enum":
        return 5 * baseMultiplier;
      case "function":
      case "method":
      case "constructor":
      case "impl":
        return 2 * baseMultiplier;
      default:
        return 1 * baseMultiplier;
    }
  }
  return 1;
}

// ── Color palette ───────────────────────────────────────────────────────────

/**
 * 12-hue Tailwind-500 palette. Symbols get colored by community
 * (community_id from F3 Leiden detection, or top-level folder hash as
 * a graceful fallback). The vivid saturation reads on the near-black
 * background in a way the slate pastels did not.
 */
export const COMMUNITY_COLORS = [
  "#ef4444", // red
  "#f97316", // orange
  "#eab308", // yellow
  "#22c55e", // green
  "#06b6d4", // cyan
  "#3b82f6", // blue
  "#8b5cf6", // violet
  "#d946ef", // fuchsia
  "#ec4899", // pink
  "#f43f5e", // rose
  "#14b8a6", // teal
  "#84cc16", // lime
] as const;

/**
 * The project root keeps its own bright accent so it always reads as
 * "the apex node" no matter which palette index its slug hashes to.
 */
const PROJECT_COLOR = "#a855f7"; // purple-500
const SYMBOL_FALLBACK = "#94a3b8"; // slate-400

/** FNV-1a 32-bit — deterministic, fast, and well-distributed for short strings. */
function fnv1a(input: string): number {
  let hash = 0x811c_9dc5;
  for (let i = 0; i < input.length; i += 1) {
    hash ^= input.charCodeAt(i);
    hash = Math.imul(hash, 0x0100_0193);
  }
  return hash >>> 0;
}

/** Parent directory of a repo-relative path, or "" for top-level paths. */
function parentDirectory(filePath: string): string {
  const idx = filePath.lastIndexOf("/");
  return idx > 0 ? filePath.slice(0, idx) : "";
}

export function colorForCommunity(communityId: string): string {
  return COMMUNITY_COLORS[fnv1a(communityId) % COMMUNITY_COLORS.length];
}

/**
 * Color routing:
 *   - Project: fixed purple accent.
 *   - Folder: hash the folder path so siblings under the same parent
 *     share a hue — the canvas reads as colored regions per top-level
 *     module instead of one indigo band.
 *   - File: hash the parent directory so all files in a folder share a
 *     color. This is the lever that breaks up the blue file wall.
 *   - Symbol: community_id (if F3 populated) → file_path's parent
 *     directory → fallback.
 */
export function colorForNode(node: SnapshotNode): string {
  if (node.kind === "folder") {
    if (/project/i.test(node.label) || node.label === "" || !node.file_path) {
      return PROJECT_COLOR;
    }
    return colorForCommunity(node.label);
  }
  if (node.kind === "file") {
    const parent = node.file_path
      ? parentDirectory(node.file_path)
      : node.label;
    if (parent.length === 0) return PROJECT_COLOR;
    return colorForCommunity(parent);
  }

  if (node.community_id) return colorForCommunity(node.community_id);
  if (node.file_path) {
    const parent = parentDirectory(node.file_path);
    if (parent.length > 0) return colorForCommunity(parent);
  }
  return SYMBOL_FALLBACK;
}


// ── Edge styling ────────────────────────────────────────────────────────────

interface EdgeStyle {
  color: string;
  sizeMultiplier: number;
  /** Drop the edge from the rendered graph entirely. */
  drop?: boolean;
}

/**
 * Per-RepoGraphEdgeKind style table. Greens for hierarchy, blue for
 * file-level deps, violet for the call graph, warm hues for the OOP
 * spine. `MemberOf` is scaffolding (introduced post-F2) — we render it
 * as the dimmest possible thread so it doesn't compete with the
 * call graph but stays available for the impact-analysis pipeline.
 */
const EDGE_STYLES: Record<string, EdgeStyle> = {
  ContainsDefinition: { color: "#2d5a3d", sizeMultiplier: 0.4 },
  DeclaredInFile: { color: "#2d5a3d", sizeMultiplier: 0.4 },
  FileReference: { color: "#1d4ed8", sizeMultiplier: 0.6 },
  SymbolReference: { color: "#7c3aed", sizeMultiplier: 0.8 },
  Reads: { color: "#0e7490", sizeMultiplier: 0.5 },
  Writes: { color: "#dc2626", sizeMultiplier: 0.6 },
  Extends: { color: "#c2410c", sizeMultiplier: 1.0 },
  Implements: { color: "#be185d", sizeMultiplier: 0.9 },
  TypeDefines: { color: "#0e7490", sizeMultiplier: 0.5 },
  Defines: { color: "#0e7490", sizeMultiplier: 0.5 },
  EntryPointOf: { color: "#10b981", sizeMultiplier: 0.7 },
  MemberOf: { color: "#1e293b", sizeMultiplier: 0.3 },
  StepInProcess: { color: "#f43f5e", sizeMultiplier: 0.7 },
};

const DEFAULT_EDGE_STYLE: EdgeStyle = { color: "#4a4a5a", sizeMultiplier: 0.5 };

export function edgeStyleFor(kind: string): EdgeStyle {
  return EDGE_STYLES[kind] ?? DEFAULT_EDGE_STYLE;
}

/** Base size scales with graph density — denser graphs get thinner strokes. */
function edgeBaseSize(nodeCount: number): number {
  if (nodeCount > 20000) return 0.4;
  if (nodeCount > 5000) return 0.6;
  return 1.0;
}

// ── Adapter ─────────────────────────────────────────────────────────────────

export interface BuildGraphOptions {
  /** Drop self-loops? Default `true`. */
  dropSelfLoops?: boolean;
  /** Drop `MemberOf` edges (scaffolding). Default `false`. */
  dropMemberOf?: boolean;
}

/**
 * Convert a snapshot payload into a graphology `Graph` configured for
 * Sigma + ForceAtlas2.
 *
 * Layout seeding strategy:
 *  - Structural nodes (file/folder) → golden-angle spiral with 15%
 *    radial jitter so the geometry doesn't look mechanical.
 *  - Symbols with `community_id` → cluster-center jitter (golden-angle
 *    distributed over 80% of the structural spread).
 *  - Symbols without community → BFS jitter around their declaring
 *    file/folder via the parent map built from `ContainsDefinition` /
 *    `DeclaredInFile` / `FileReference`.
 *  - Orphans → random within half the structural spread.
 */
export function buildGraphFromSnapshot(
  snapshot: SnapshotPayload,
  options: BuildGraphOptions = {},
): Graph {
  const dropSelfLoops = options.dropSelfLoops ?? true;
  const dropMemberOf = options.dropMemberOf ?? false;
  const graph = new Graph({ multi: true, type: "directed" });

  const nodes = snapshot.nodes;
  const nodeCount = nodes.length;
  const ranks = nodes.map((n) => n.pagerank);
  const maxRank = ranks.length > 0 ? Math.max(...ranks, 0.000_001) : 1;

  const structuralSpread = Math.sqrt(Math.max(nodeCount, 1)) * 40;
  const childJitter = Math.sqrt(Math.max(nodeCount, 1)) * 3;
  const clusterJitter = Math.sqrt(Math.max(nodeCount, 1)) * 1.5;

  const nodeMap = new Map(nodes.map((n) => [n.id, n]));

  // Build parent → children map from hierarchy edges. Only structural
  // / declaration relationships count as "parent owns child" — call
  // graph edges (SymbolReference) deliberately don't influence layout
  // since they're noise during seeding.
  const HIERARCHY_KINDS = new Set([
    "ContainsDefinition",
    "DeclaredInFile",
    "FileReference",
  ]);
  const childToParent = new Map<string, string>();
  for (const edge of snapshot.edges) {
    if (!HIERARCHY_KINDS.has(edge.kind)) continue;
    if (!nodeMap.has(edge.from) || !nodeMap.has(edge.to)) continue;
    if (!childToParent.has(edge.to)) childToParent.set(edge.to, edge.from);
  }
  const parentToChildren = new Map<string, string[]>();
  for (const [child, parent] of childToParent) {
    const list = parentToChildren.get(parent) ?? [];
    list.push(child);
    parentToChildren.set(parent, list);
  }

  const structuralNodes = nodes.filter(
    (n) => n.kind === "folder" || n.kind === "file",
  );

  // Cluster centers — golden-angle distributed; sqrt(idx) radius
  // produces an even areal density rather than a compressed center.
  const clusterCenters = new Map<string, { x: number; y: number }>();
  const communityIds = new Set<string>();
  for (const n of nodes) if (n.community_id) communityIds.add(n.community_id);
  if (communityIds.size > 0) {
    const clusterSpread = structuralSpread * 0.8;
    const goldenAngle = Math.PI * (3 - Math.sqrt(5));
    const total = communityIds.size;
    let i = 0;
    for (const cid of communityIds) {
      const angle = i * goldenAngle;
      const radius = clusterSpread * Math.sqrt((i + 1) / total);
      clusterCenters.set(cid, {
        x: radius * Math.cos(angle),
        y: radius * Math.sin(angle),
      });
      i += 1;
    }
  }

  const positions = new Map<string, { x: number; y: number }>();

  // Structural nodes go down first — their children cluster around them.
  const structuralCount = Math.max(structuralNodes.length, 1);
  const goldenAngle = Math.PI * (3 - Math.sqrt(5));
  structuralNodes.forEach((node, index) => {
    const angle = index * goldenAngle;
    const radius =
      structuralSpread * Math.sqrt((index + 1) / structuralCount);
    const jitter = structuralSpread * 0.15;
    const x = radius * Math.cos(angle) + (Math.random() - 0.5) * jitter;
    const y = radius * Math.sin(angle) + (Math.random() - 0.5) * jitter;
    positions.set(node.id, { x, y });
    addNode(graph, node, { x, y }, maxRank, nodeCount);
  });

  const SYMBOL_CLUSTER_KINDS = new Set([
    "function",
    "method",
    "class",
    "struct",
    "interface",
    "enum",
    "constructor",
    "trait",
    "impl",
  ]);

  const placeNode = (id: string) => {
    if (graph.hasNode(id)) return;
    const node = nodeMap.get(id);
    if (!node) return;

    let pos: { x: number; y: number } | null = null;
    const cid = node.community_id;
    const isClusterableSymbol =
      node.kind === "symbol" && SYMBOL_CLUSTER_KINDS.has(node.symbol_kind ?? "");

    if (isClusterableSymbol && cid && clusterCenters.has(cid)) {
      const c = clusterCenters.get(cid)!;
      pos = {
        x: c.x + (Math.random() - 0.5) * clusterJitter,
        y: c.y + (Math.random() - 0.5) * clusterJitter,
      };
    } else {
      const parentId = childToParent.get(id);
      const parentPos = parentId ? positions.get(parentId) : null;
      if (parentPos) {
        pos = {
          x: parentPos.x + (Math.random() - 0.5) * childJitter,
          y: parentPos.y + (Math.random() - 0.5) * childJitter,
        };
      } else {
        pos = {
          x: (Math.random() - 0.5) * structuralSpread * 0.5,
          y: (Math.random() - 0.5) * structuralSpread * 0.5,
        };
      }
    }
    positions.set(id, pos);
    addNode(graph, node, pos, maxRank, nodeCount);
  };

  // BFS from structural nodes so parents always exist before children.
  const queue: string[] = [...structuralNodes.map((n) => n.id)];
  const visited = new Set<string>(queue);
  while (queue.length > 0) {
    const cur = queue.shift()!;
    const children = parentToChildren.get(cur) ?? [];
    for (const childId of children) {
      if (visited.has(childId)) continue;
      visited.add(childId);
      placeNode(childId);
      queue.push(childId);
    }
  }
  for (const node of nodes) {
    if (!graph.hasNode(node.id)) placeNode(node.id);
  }

  // Edges — per-kind colors, base scaled by graph density, modulated
  // by per-edge confidence so hand-resolved edges trail brighter than
  // weak heuristic ones.
  const baseSize = edgeBaseSize(nodeCount);
  for (const edge of snapshot.edges) {
    if (dropMemberOf && edge.kind === "MemberOf") continue;
    if (!graph.hasNode(edge.from) || !graph.hasNode(edge.to)) continue;
    if (dropSelfLoops && edge.from === edge.to) continue;
    const style = edgeStyleFor(edge.kind);
    if (style.drop) continue;
    const confidenceFactor = 0.4 + edge.confidence * 0.6;
    graph.addEdge(edge.from, edge.to, {
      kind: edge.kind,
      confidence: edge.confidence,
      reason: edge.reason,
      size: baseSize * style.sizeMultiplier * confidenceFactor,
      color: style.color,
      type: "curved",
      curvature: 0.12 + Math.random() * 0.08,
    });
  }

  return graph;
}

function addNode(
  graph: Graph,
  node: SnapshotNode,
  pos: { x: number; y: number },
  maxRank: number,
  nodeCount: number,
): void {
  if (graph.hasNode(node.id)) return;
  const normalized = node.pagerank / maxRank;
  const size = scaledNodeSize(node, normalized, nodeCount);
  graph.addNode(node.id, {
    label: node.label,
    x: pos.x,
    y: pos.y,
    size,
    color: colorForNode(node),
    mass: massForNode(node, nodeCount),
    kind: node.kind,
    symbolKind: node.symbol_kind,
    pagerank: node.pagerank,
    filePath: node.file_path,
    communityId: node.community_id,
  });
}

/**
 * Visual size with a hierarchy floor: structural nodes stay readable
 * even on huge graphs, symbols shrink toward 2px so they don't drown
 * the canvas. Pagerank then tilts within the per-kind band.
 */
function scaledNodeSize(
  node: SnapshotNode,
  pagerankNormalized: number,
  nodeCount: number,
): number {
  const base = baseNodeSize(node);
  const scaled = densityScale(base, nodeCount);
  return scaled + pagerankNormalized * Math.max(scaled * 0.6, 1.5);
}

function baseNodeSize(node: SnapshotNode): number {
  if (node.kind === "folder") {
    if (/project/i.test(node.label)) return 20;
    return 10;
  }
  if (node.kind === "file") return 6;
  if (node.kind === "symbol") {
    switch (node.symbol_kind) {
      case "class":
      case "struct":
      case "record":
        return 8;
      case "interface":
      case "trait":
        return 7;
      case "enum":
      case "union":
        return 5;
      case "function":
      case "constructor":
        return 4;
      case "method":
      case "impl":
        return 3;
      case "variable":
      case "const":
      case "static":
      case "property":
        return 2;
      case "import":
        return 1.5;
      default:
        return 3;
    }
  }
  return 4;
}

function densityScale(base: number, nodeCount: number): number {
  if (nodeCount > 50000) return Math.max(1, base * 0.4);
  if (nodeCount > 20000) return Math.max(1.5, base * 0.5);
  if (nodeCount > 5000) return Math.max(2, base * 0.65);
  if (nodeCount > 1000) return Math.max(2.5, base * 0.8);
  return base;
}
