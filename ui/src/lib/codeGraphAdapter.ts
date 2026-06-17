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

export type SnapshotNodeKind = "file" | "folder" | "symbol" | "community";

export interface SnapshotNode {
  id: string;
  kind: SnapshotNodeKind;
  label: string;
  symbol_kind?: string;
  file_path?: string;
  pagerank: number;
  community_id?: string;
  /**
   * Semantic-zoom community metadata (server `SnapshotNode` wire shape).
   * Only populated for collapsed `kind: "community"` nodes emitted by a
   * `level=community` snapshot. Symbol/file/folder nodes leave these
   * undefined so the existing wire shape is unchanged.
   */
  member_count?: number;
  internal_edge_count?: number;
  workspace_kind?: string;
  workspace?: string;
  /**
   * True when included only to preserve a selected workspace's external edge
   * context.
   */
  workspace_context?: boolean;
  /**
   * Iter 30: per-function cognitive complexity from the tree-sitter
   * walker. Only populated for function-like nodes (function/method/
   * constructor) and only when the language is in the walker's table.
   * `undefined` for files, types, externals, synthetic nodes — the
   * heatmap mode renders those as muted gray so non-function nodes
   * don't dominate the eye.
   */
  cognitive?: number;
  /**
   * v10: true when this node is a test (File whose path matches the test
   * convention, or a Symbol defined in such a file / SCIP `Test` role).
   * Drives the "hide tests" toolbar toggle.
   */
  is_test?: boolean;
  /**
   * Server-precomputed layout coordinate, computed in `derive_graph_caches`
   * during warm alongside PageRank / communities / SCCs. When every node
   * in the snapshot carries finite `x`/`y` values, the UI uses them as the
   * graphology node positions directly and skips ForceAtlas2 — see
   * `hasPrecomputedCoordinates` and `buildGraphFromSnapshot`.
   *
   * `undefined` (or any non-finite value) means the server did not ship a
   * layout for this node; the client falls back to the golden-angle /
   * cluster-center seeding path so FA2 still has a starting position.
   */
  x?: number;
  y?: number;
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

/**
 * Filter a full project snapshot to one workspace without losing the
 * cross-workspace relationships that make the local nodes understandable.
 *
 * The server snapshot is intentionally fetched once for the project. When the
 * user chooses a workspace we keep nodes tagged with that workspace, then keep
 * any edge touching one of those nodes and pull in the remote endpoint node as
 * `workspace_context` so downstream rendering can de-emphasize it without
 * dropping the relationship entirely.
 */
export function filterSnapshotForWorkspace(
  snapshot: SnapshotPayload,
  workspaceSlug: string | null | undefined,
): SnapshotPayload {
  const slug = workspaceSlug?.trim();
  if (!slug) return snapshot;

  const nodeById = new Map(snapshot.nodes.map((node) => [node.id, node]));
  const selectedNodeIds = new Set(
    snapshot.nodes
      .filter((node) => node.workspace === slug)
      .map((node) => node.id),
  );

  const includedNodeIds = new Set<string>(selectedNodeIds);
  const edges = snapshot.edges.filter((edge) => {
    const touchesSelected =
      selectedNodeIds.has(edge.from) || selectedNodeIds.has(edge.to);
    if (!touchesSelected) return false;
    if (!nodeById.has(edge.from) || !nodeById.has(edge.to)) return false;
    includedNodeIds.add(edge.from);
    includedNodeIds.add(edge.to);
    return true;
  });

  const nodes = snapshot.nodes
    .filter((node) => includedNodeIds.has(node.id))
    .map((node) => {
      if (selectedNodeIds.has(node.id)) {
        const selectedNode = { ...node };
        delete selectedNode.workspace_context;
        return selectedNode;
      }
      return { ...node, workspace_context: true };
    });

  return {
    ...snapshot,
    nodes,
    edges,
    total_nodes: nodes.length,
    total_edges: edges.length,
    truncated: false,
  };
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
          member_count:
            typeof n.member_count === "number" &&
            Number.isFinite(n.member_count) &&
            n.member_count >= 0
              ? Math.floor(n.member_count)
              : undefined,
          internal_edge_count:
            typeof n.internal_edge_count === "number" &&
            Number.isFinite(n.internal_edge_count) &&
            n.internal_edge_count >= 0
              ? Math.floor(n.internal_edge_count)
              : undefined,
          workspace_kind: nonEmptyString(n.workspace_kind),
          workspace: nonEmptyString(n.workspace),
          workspace_context: n.workspace_context === true,
          cognitive:
            typeof n.cognitive === "number" && Number.isFinite(n.cognitive)
              ? n.cognitive
              : undefined,
          is_test: n.is_test === true,
          x:
            typeof n.x === "number" && Number.isFinite(n.x) ? n.x : undefined,
          y:
            typeof n.y === "number" && Number.isFinite(n.y) ? n.y : undefined,
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

function nonEmptyString(value: unknown): string | undefined {
  return typeof value === "string" && value.trim().length > 0
    ? value.trim()
    : undefined;
}

function normalizeKind(value: unknown): SnapshotNodeKind {
  if (
    value === "folder" ||
    value === "file" ||
    value === "symbol" ||
    value === "community"
  ) {
    return value;
  }
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
 * Bounded visual scale for collapsed community nodes based on their
 * `member_count`. Communities aggregate many symbol-level nodes into one
 * blob, so they should read larger than any individual symbol — but the
 * scale must stay bounded so a 50k-member community doesn't swallow the
 * canvas.
 *
 * Uses `log10(member_count + 1)` mapped into `[COMMUNITY_MIN_SIZE,
 * COMMUNITY_MAX_SIZE]`. The +1 makes a 1-member community sit at the
 * floor, and the log curve keeps a 10k-member community only ~4× the
 * floor rather than 10k× it.
 */
export const COMMUNITY_MIN_SIZE = 12;
export const COMMUNITY_MAX_SIZE = 60;
const LOG10_MAX_MEMBER = Math.log10(10_000 + 1);

export function communityNodeSize(memberCount: number | undefined): number {
  const count =
    typeof memberCount === "number" &&
    Number.isFinite(memberCount) &&
    memberCount > 0
      ? memberCount
      : 1;
  const t = Math.min(Math.log10(count + 1) / LOG10_MAX_MEMBER, 1);
  return COMMUNITY_MIN_SIZE + t * (COMMUNITY_MAX_SIZE - COMMUNITY_MIN_SIZE);
}

/**
 * Bounded FA2 mass for collapsed community nodes. Communities act as
 * heavy gravity wells (heavier than folders) so the collapsed graph
 * spreads apart and the inter-community edges stay legible. Scaled with
 * a bounded log curve so mass stays proportional to the visual size.
 */
export function communityNodeMass(memberCount: number | undefined): number {
  const size = communityNodeSize(memberCount);
  // Map the [12, 60] size band onto a [40, 120] mass band so communities
  // outweigh folders (15) and project roots (50) without exploding FA2.
  const min = 40;
  const max = 120;
  const t =
    (size - COMMUNITY_MIN_SIZE) / (COMMUNITY_MAX_SIZE - COMMUNITY_MIN_SIZE);
  return min + t * (max - min);
}

/**
 * Heavy structural masses act as gravity wells in FA2 — folders blast
 * apart and pull their files with them, producing the cluster spread
 * GitNexus relies on. Scaled with node count so 12k-node monorepos
 * still spread instead of collapsing.
 */
export function massForNode(node: SnapshotNode, nodeCount: number = 0): number {
  const baseMultiplier = nodeCount > 5000 ? 2 : nodeCount > 1000 ? 1.5 : 1;

  if (node.kind === "community") {
    return communityNodeMass(node.member_count);
  }
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
 * Distinct, deterministic workspace identity palette. Kept separate from the
 * topology/community palette so workspace membership can be shown via halos /
 * badges without stealing the main color channel from topology or complexity
 * heatmap modes.
 */
export const WORKSPACE_COLORS = [
  "#22d3ee", // cyan-400
  "#f472b6", // pink-400
  "#a3e635", // lime-400
  "#fb923c", // orange-400
  "#818cf8", // indigo-400
  "#34d399", // emerald-400
  "#facc15", // yellow-400
  "#c084fc", // purple-400
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

export function colorForWorkspace(workspace: string): string {
  return WORKSPACE_COLORS[fnv1a(workspace) % WORKSPACE_COLORS.length];
}

function workspaceBadge(workspace: string): string {
  const parts = workspace
    .split(/[^a-zA-Z0-9]+/)
    .map((part) => part.trim())
    .filter(Boolean);
  const seed = parts.length > 0 ? parts : [workspace];
  return seed
    .slice(0, 2)
    .map((part) => part[0]?.toUpperCase() ?? "")
    .join("")
    .slice(0, 3);
}

/**
 * Color routing:
 *   - Project: fixed purple accent.
 *   - Folder: hash the folder path so siblings under the same parent
 *     share a hue — the canvas reads as colored regions per top-level
 *     module instead of one indigo band.
 *   - File: hash the parent directory so all files in a folder share a
 *     color. This is the lever that breaks up the blue file wall.
 *   - Community: hash the stable `community_id` so each collapsed blob
 *     gets a distinct, deterministic hue. Falls back to the label when
 *     the id is absent (shouldn't happen for real server payloads).
 *   - Symbol: community_id (if F3 populated) → file_path's parent
 *     directory → fallback.
 */
export function colorForNode(node: SnapshotNode): string {
  if (node.kind === "community") {
    if (node.community_id) return colorForCommunity(node.community_id);
    return colorForCommunity(node.label || node.id);
  }
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
 * Graph-level attribute name set by `buildGraphFromSnapshot` when the
 * snapshot's positions are used verbatim. `useSigmaGraph` reads this on
 * the follow-up task to gate `FA2LayoutSupervisor.start()` — if every
 * node already sits at a server-computed coordinate, FA2 has nothing to
 * converge on and would just churn the canvas.
 */
export const PRECOMPUTED_LAYOUT_ATTRIBUTE = "precomputedLayout";

/**
 * Returns true when every node in `snapshot` carries a finite numeric
 * `x` and `y`. Empty snapshots are vacuously complete (there are no
 * nodes to check), so the helper returns `true` — `buildGraphFromSnapshot`
 * will simply emit an empty graphology graph with the precomputed flag
 * set, which is the cheapest possible outcome.
 *
 * Any missing / non-numeric / non-finite coordinate forces a fallback to
 * the golden-angle + cluster-center + BFS-jitter seed path so FA2 still
 * has a starting layout. This is the reliability floor: partial
 * coordinates from an old artifact, NaN from a buggy client, or missing
 * fields from a stale cache must all degrade to the existing seed path
 * rather than render a half-positioned graph.
 */
export function hasPrecomputedCoordinates(snapshot: SnapshotPayload): boolean {
  if (!snapshot.nodes || snapshot.nodes.length === 0) return true;
  for (const node of snapshot.nodes) {
    if (typeof node.x !== "number" || !Number.isFinite(node.x)) return false;
    if (typeof node.y !== "number" || !Number.isFinite(node.y)) return false;
  }
  return true;
}

/**
 * Convert a snapshot payload into a graphology `Graph` configured for
 * Sigma + ForceAtlas2.
 *
 * Layout seeding strategy:
 *  - **Precomputed**: when every node carries a finite `x`/`y` (see
 *    `hasPrecomputedCoordinates`), positions are used verbatim, the
 *    golden-angle / cluster / BFS seed path is skipped, and the graph is
 *    flagged with `precomputedLayout: true` so `useSigmaGraph` can skip
 *    the FA2 supervisor. The server computes the layout once during warm
 *    (see `derive_graph_caches`) and ships the result in the artifact.
 *  - **Seeded fallback** (no precomputed coordinates): structural nodes
 *    (file/folder) → golden-angle spiral with 15% radial jitter; symbols
 *    with `community_id` → cluster-center jitter (golden-angle over 80%
 *    of the structural spread); symbols without community → BFS jitter
 *    around their declaring file/folder; orphans → random within half
 *    the structural spread.
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

  // Precomputed branch — when every node ships a finite (x, y), the
  // server has already done the warm-time layout work and FA2 has
  // nothing to converge on. Use the coordinates verbatim, mark the
  // graph so `useSigmaGraph` can skip the FA2 supervisor, and exit
  // before the structural / golden-angle / community / random seed
  // branch below. The seed path is intentionally untouched for the
  // fallback case so existing 4xx / 5xx / pre-warm snapshots render
  // exactly as before.
  if (nodeCount > 0 && hasPrecomputedCoordinates(snapshot)) {
    for (const node of nodes) {
      // hasPrecomputedCoordinates proved node.x / node.y are finite
      // numbers, so the non-null assertions are safe at runtime.
      addNode(
        graph,
        node,
        { x: node.x!, y: node.y! },
        maxRank,
        nodeCount,
      );
    }
    addEdgesFromSnapshot(graph, snapshot, nodeCount, {
      dropSelfLoops,
      dropMemberOf,
    });
    graph.setAttribute(PRECOMPUTED_LAYOUT_ATTRIBUTE, true);
    return graph;
  }

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
    (n) =>
      n.kind === "folder" ||
      n.kind === "file" ||
      n.kind === "community",
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
  // weak heuristic ones. Extracted so the precomputed-coords branch
  // above reuses the same edge-rendering pipeline.
  addEdgesFromSnapshot(graph, snapshot, nodeCount, {
    dropSelfLoops,
    dropMemberOf,
  });

  return graph;
}

function addEdgesFromSnapshot(
  graph: Graph,
  snapshot: SnapshotPayload,
  nodeCount: number,
  options: { dropSelfLoops: boolean; dropMemberOf: boolean },
): void {
  const { dropSelfLoops, dropMemberOf } = options;
  const nodeMap = new Map(snapshot.nodes.map((n) => [n.id, n]));
  const baseSize = edgeBaseSize(nodeCount);
  for (const edge of snapshot.edges) {
    if (dropMemberOf && edge.kind === "MemberOf") continue;
    if (!graph.hasNode(edge.from) || !graph.hasNode(edge.to)) continue;
    if (dropSelfLoops && edge.from === edge.to) continue;
    const style = edgeStyleFor(edge.kind);
    if (style.drop) continue;
    const confidenceFactor = 0.4 + edge.confidence * 0.6;
    const sourceWorkspace = nodeMap.get(edge.from)?.workspace;
    const targetWorkspace = nodeMap.get(edge.to)?.workspace;
    const isCrossWorkspace =
      !!sourceWorkspace && !!targetWorkspace && sourceWorkspace !== targetWorkspace;
    const crossWorkspaceColor = isCrossWorkspace ? "#facc15" : undefined;
    graph.addEdge(edge.from, edge.to, {
      kind: edge.kind,
      confidence: edge.confidence,
      reason: edge.reason,
      sourceWorkspace,
      targetWorkspace,
      isCrossWorkspace,
      crossWorkspace: isCrossWorkspace,
      size: baseSize * style.sizeMultiplier * confidenceFactor * (isCrossWorkspace ? 2.4 : 1),
      color: crossWorkspaceColor ?? style.color,
      type: "curved",
      curvature: (isCrossWorkspace ? 0.28 : 0.12) + Math.random() * 0.08,
      zIndex: isCrossWorkspace ? 20 : 1,
      lineStyle: isCrossWorkspace ? "dashed" : "solid",
    });
  }
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
  const workspaceColor = node.workspace ? colorForWorkspace(node.workspace) : undefined;
  const label = node.workspace ? `${node.label} · ${node.workspace}` : node.label;
  graph.addNode(node.id, {
    label,
    baseLabel: node.label,
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
    memberCount: node.member_count,
    internalEdgeCount: node.internal_edge_count,
    workspaceKind: node.workspace_kind,
    workspace: node.workspace,
    workspaceColor,
    workspaceBadge: node.workspace ? workspaceBadge(node.workspace) : undefined,
    borderColor: workspaceColor,
    borderSize: workspaceColor ? (node.workspace_context === true ? 1 : 1.5) : undefined,
    haloed: workspaceColor ? true : undefined,
    isWorkspaceContext: node.workspace_context === true,
    /**
     * Iter 30: forwarded so the heatmap reducer can colorize without
     * a side lookup. `undefined` means "non-function or unsupported
     * language" — heatmap mode bins those into the gray bucket.
     */
    cognitive: node.cognitive,
    /** v10: forwarded so the "hide tests" reducer can hide test nodes. */
    isTest: node.is_test === true,
    /** Stash the topology color so we can restore it when toggling modes. */
    topologyColor: colorForNode(node),
  });
}

/**
 * Visual size with a hierarchy floor: structural nodes stay readable
 * even on huge graphs, symbols shrink toward 2px so they don't drown
 * the canvas. Pagerank then tilts within the per-kind band.
 *
 * Community nodes bypass the per-kind / density / pagerank path: their
 * size is driven purely by the bounded member-count scale
 * (`communityNodeSize`) so a collapsed graph reads as a handful of
 * legible blobs rather than uniformly tiny dots.
 */
function scaledNodeSize(
  node: SnapshotNode,
  pagerankNormalized: number,
  nodeCount: number,
): number {
  if (node.kind === "community") {
    return communityNodeSize(node.member_count);
  }
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

// ── Community expand / collapse (semantic zoom) ───────────────────────────

/**
 * Maximum inter-click interval (ms) for two `clickNode` events to count
 * as a double-click. Sigma 3 exposes a `doubleClickNode` event via its
 * double-click captor, but the installed version's event surface varies;
 * the canvas uses a timestamp/last-node guard around `clickNode` with
 * this interval so behavior is deterministic regardless of version.
 */
export const DOUBLE_CLICK_INTERVAL_MS = 350;

/**
 * Pure double-click detector. Used by the canvas to implement the
 * timestamp/last-node guard around `clickNode` (see the task design:
 * "implement a timestamp/last-node guard around clickNode with a small
 * interval"). Kept as a pure exported helper so focused tests can pin
 * the interval + same-node requirement without rendering the canvas.
 *
 * Two clicks count as a double-click when:
 *   - the second click hits the *same* node id as the first, and
 *   - the interval between them is at most `interval` ms (default
 *     {@link DOUBLE_CLICK_INTERVAL_MS}).
 *
 * Returns `true` for the second click and `false` for the first click
 * (or any click outside the window / on a different node). The caller
 * records the returned "last" tuple so consecutive single clicks on the
 * same node outside the window don't accumulate.
 */
export function isDoubleClick(
  prev: { nodeId: string; at: number } | null,
  nodeId: string,
  now: number,
  interval: number = DOUBLE_CLICK_INTERVAL_MS,
): boolean {
  if (!prev) return false;
  if (prev.nodeId !== nodeId) return false;
  return now - prev.at <= interval;
}

/**
 * Build a snapshot with a community replaced by its member symbol nodes.
 *
 * The caller supplies:
 *   - `communitySnapshot`: the community-level snapshot (collapsed view)
 *     that the canvas is currently rendering.
 *   - `symbolSnapshot`: a symbol-level snapshot for the same project.
 *   - `communityId`: the stable `community_id` of the community to expand.
 *
 * The helper:
 *   1. Removes the community node identified by `communityId`.
 *   2. Adds every symbol node from `symbolSnapshot` whose `community_id`
 *      matches `communityId`.
 *   3. Adds all symbol-level edges whose *both* endpoints are member
 *      nodes of the expanded community (intra-community edges).
 *   4. Preserves aggregated inter-community edges from the community
 *      snapshot for all *other* communities. Edges touching the expanded
 *      community node are dropped (the individual members now carry
 *      those relationships, but exact member↔community edge mapping is
 *      not available without a server community-scope parameter, so this
 *      is the documented best-effort behavior).
 *
 * Edges whose endpoints are no longer present after the splice are
 * dropped, exactly as `buildGraphFromSnapshot` does.
 */
export function expandCommunityInSnapshot(
  communitySnapshot: SnapshotPayload,
  symbolSnapshot: SnapshotPayload,
  communityId: string,
): SnapshotPayload {
  const communityNode = findCommunityNode(communitySnapshot, communityId);
  const communityNodeId = communityNode ? communityNode.id : communityId;

  const members = symbolSnapshot.nodes.filter(
    (n) => n.community_id === communityId,
  );
  const memberIds = new Set(members.map((n) => n.id));

  const otherCommunities = communitySnapshot.nodes.filter(
    (n) => n.id !== communityNodeId,
  );

  // Intra-community edges: both endpoints are members of the expanded community.
  const memberEdges = symbolSnapshot.edges.filter(
    (e) => memberIds.has(e.from) && memberIds.has(e.to),
  );

  // Inter-community edges for the *other* (still-collapsed) communities.
  // Edges touching the expanded community node are dropped — the individual
  // members now represent that community, but there's no server mapping
  // from an aggregated edge back to specific member endpoints.
  const otherCommunityEdges = communitySnapshot.edges.filter(
    (e) => e.from !== communityNodeId && e.to !== communityNodeId,
  );

  const nodes = [...otherCommunities, ...members];
  const edges = [...otherCommunityEdges, ...memberEdges];

  return {
    ...communitySnapshot,
    nodes,
    edges,
    total_nodes: nodes.length,
    total_edges: edges.length,
  };
}

/**
 * Build a snapshot with an expanded community collapsed back into its
 * community node. Restores the original community node and the aggregated
 * inter-community edges from the community-level snapshot.
 *
 * The caller supplies:
 *   - `communitySnapshot`: the community-level snapshot (collapsed view).
 *   - `expandedSnapshot`: the currently-rendered snapshot (community view
 *     with one community expanded into members).
 *   - `communityId`: the stable `community_id` of the community to collapse.
 *
 * The helper:
 *   1. Removes every symbol node whose `community_id` matches `communityId`.
 *   2. Re-inserts the original community node from `communitySnapshot`.
 *   3. Restores the aggregated inter-community edges from
 *      `communitySnapshot` (both edges between other communities and
 *      edges touching the restored community node).
 *   4. Drops symbol-level intra-community edges for the collapsed
 *      community (they're now aggregated inside the restored node).
 *
 * Other still-expanded communities in `expandedSnapshot` are preserved
 * so collapsing one community doesn't collapse siblings the user has
 * also expanded.
 */
export function collapseCommunityInSnapshot(
  communitySnapshot: SnapshotPayload,
  expandedSnapshot: SnapshotPayload,
  communityId: string,
): SnapshotPayload {
  const communityNode = findCommunityNode(communitySnapshot, communityId);
  const nodeId = communityNode ? communityNode.id : communityId;

  // Remove symbol members of the community being collapsed, and the
  // expanded community node id if it somehow lingers.
  const remainingNodes = expandedSnapshot.nodes.filter(
    (n) => n.community_id !== communityId && n.id !== nodeId,
  );

  // Re-insert the original community node.
  const nodes = communityNode
    ? [...remainingNodes, communityNode]
    : remainingNodes;
  const nodeIdSet = new Set(nodes.map((n) => n.id));

  // Restore aggregated inter-community edges from the community snapshot
  // that are valid for the current node set.
  const restoredEdges = communitySnapshot.edges.filter(
    (e) => nodeIdSet.has(e.from) && nodeIdSet.has(e.to),
  );

  // Keep symbol-level edges for *other* still-expanded communities:
  // drop edges touching any member of the collapsed community.
  const memberIds = new Set(
    expandedSnapshot.nodes
      .filter((n) => n.community_id === communityId)
      .map((n) => n.id),
  );
  const otherExpandedEdges = expandedSnapshot.edges.filter(
    (e) => !memberIds.has(e.from) && !memberIds.has(e.to),
  );

  // Merge: restored community edges take precedence; add symbol edges
  // for other expanded communities that aren't already represented.
  const seen = new Set(
    restoredEdges.map((e) => `${e.from}\u0000${e.to}\u0000${e.kind}`),
  );
  const edges = [...restoredEdges];
  for (const e of otherExpandedEdges) {
    const key = `${e.from}\u0000${e.to}\u0000${e.kind}`;
    if (seen.has(key)) continue;
    if (!nodeIdSet.has(e.from) || !nodeIdSet.has(e.to)) continue;
    seen.add(key);
    edges.push(e);
  }

  return {
    ...communitySnapshot,
    nodes,
    edges,
    total_nodes: nodes.length,
    total_edges: edges.length,
  };
}

/**
 * Find the community node in a community-level snapshot whose stable
 * `community_id` matches `communityId`. Returns the node (or `undefined`
 * when no community node carries that id).
 */
function findCommunityNode(
  snapshot: SnapshotPayload,
  communityId: string,
): SnapshotNode | undefined {
  return snapshot.nodes.find(
    (n) => n.kind === "community" && n.community_id === communityId,
  );
}
