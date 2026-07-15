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
import { computeForceLayout } from "@/lib/codeGraphLayouts";

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
   * Server-derived topical keywords for collapsed community nodes. Undefined on
   * legacy snapshots and non-community nodes.
   */
  keywords?: string[];
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
  /**
   * Proposal lmkv: server-precomputed 3D "galaxy" layout coordinates, computed
   * once at warm time alongside `x`/`y`. When every rendered node carries
   * finite `gx`/`gy`/`gz`, the galaxy view uses them directly and skips its Web
   * Worker force layout entirely. `undefined` (legacy blobs, the gitignored
   * Storybook fixture) means the client falls back to the worker path.
   */
  gx?: number;
  gy?: number;
  gz?: number;
  /**
   * Proposal lmkv: server-precomputed node degree from the collapsed galaxy
   * edge view. Used in place of the client's recomputation when present.
   */
  degree?: number;
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
          file_path: typeof n.file_path === "string" ? n.file_path : undefined,
          pagerank: Number(n.pagerank ?? 0),
          community_id:
            typeof n.community_id === "string" ? n.community_id : undefined,
          keywords: normalizeKeywords(n.keywords),
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
          x: typeof n.x === "number" && Number.isFinite(n.x) ? n.x : undefined,
          y: typeof n.y === "number" && Number.isFinite(n.y) ? n.y : undefined,
          gx: typeof n.gx === "number" && Number.isFinite(n.gx) ? n.gx : undefined,
          gy: typeof n.gy === "number" && Number.isFinite(n.gy) ? n.gy : undefined,
          gz: typeof n.gz === "number" && Number.isFinite(n.gz) ? n.gz : undefined,
          degree:
            typeof n.degree === "number" &&
            Number.isFinite(n.degree) &&
            n.degree >= 0
              ? Math.floor(n.degree)
              : undefined,
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

function normalizeKeywords(value: unknown): string[] | undefined {
  if (!Array.isArray(value)) return undefined;
  const keywords = value
    .map(nonEmptyString)
    .filter((keyword): keyword is string => keyword !== undefined);
  return keywords.length > 0 ? keywords : undefined;
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
  const tail = descriptor.replace(/\(\)\.$/, "()").replace(/[#.[\]]+$/, "");
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

/**
 * Palette for the unified topology grouping (crate → community → folder).
 * The two base palettes concatenated give ~20 distinct hues, enough that a
 * monorepo's crates mostly each get their own colour. Vivid -500s first,
 * lighter -400s after, all legible on the near-black canvas.
 */
export const GROUP_COLORS = [...COMMUNITY_COLORS, ...WORKSPACE_COLORS] as const;

/**
 * The crate / top-level module a repo-relative path belongs to. For a cargo
 * workspace, `…/crates/<name>/…` → `<name>` (the actual crate); otherwise the
 * first path segment (`ui`, `server`, …). This is the lever that fixes "all
 * one colour": the server-side `workspace` tag is the coarse workspace member
 * (`"server"` for everything under `server/crates/*`), so colouring by it
 * paints the whole monorepo one hue. Empty string for blank input.
 */
export function crateForPath(filePath: string): string {
  const m = /(?:^|\/)crates\/([^/]+)(?:\/|$)/.exec(filePath);
  if (m) return m[1];
  const slash = filePath.indexOf("/");
  return slash > 0 ? filePath.slice(0, slash) : filePath;
}

/**
 * Unified grouping key for topology colour, in the user's mental model:
 * "crate if we have it, else community/folder — same thing." Prefer the crate
 * derived from the file path, fall back to the detected community, then the
 * parent folder, then the node's workspace tag. `null` when nothing applies
 * (caller picks a neutral fallback).
 */
export function colorGroupKey(node: SnapshotNode): string | null {
  if (node.file_path) {
    const crate = crateForPath(node.file_path);
    if (crate.length > 0) return crate;
  }
  if (node.community_id) return node.community_id;
  if (node.file_path) {
    const parent = parentDirectory(node.file_path);
    if (parent.length > 0) return parent;
  }
  if (node.workspace) return node.workspace;
  return null;
}

/** Deterministic hue for a grouping key from the {@link GROUP_COLORS} palette. */
export function colorForGroup(key: string): string {
  return GROUP_COLORS[fnv1a(key) % GROUP_COLORS.length];
}

/** One row of the crate legend: a grouping key, its hue, and node count. */
export interface CrateLegendEntry {
  key: string;
  color: string;
  count: number;
}

/**
 * Tally the topology grouping keys present on a built graph into legend rows
 * (key + hue + count), sorted by count desc then key for stable ordering.
 * Reads the `colorGroup` attribute stamped by {@link addNode}, so the legend
 * matches exactly how the dots are colored. Pure + exported for testing.
 */
export function computeCrateLegend(graph: Graph): CrateLegendEntry[] {
  const counts = new Map<string, number>();
  graph.forEachNode((_id, attrs) => {
    const key = attrs.colorGroup;
    if (typeof key === "string" && key.length > 0) {
      counts.set(key, (counts.get(key) ?? 0) + 1);
    }
  });
  return Array.from(counts, ([key, count]) => ({
    key,
    color: colorForGroup(key),
    count,
  })).sort((a, b) =>
    b.count !== a.count ? b.count - a.count : a.key.localeCompare(b.key, "en"),
  );
}

/**
 * Key with the highest count in a tally map, ties broken by lexical order
 * for determinism. Returns `undefined` for an empty map. Used to pick a
 * community hull's dominant crate.
 */
function dominantKey(counts: Map<string, number>): string | undefined {
  let best: string | undefined;
  let bestCount = -1;
  for (const [key, count] of counts) {
    if (count > bestCount || (count === bestCount && best !== undefined && key < best)) {
      best = key;
      bestCount = count;
    }
  }
  return best;
}

// ── Community hull metadata ────────────────────────────────────────────────

/**
 * Graph-level attribute name under which `buildGraphFromSnapshot` stores the
 * derived `CommunityHull[]`. The canvas reads this to draw background hull
 * regions behind member nodes — communities are no longer clickable graph
 * nodes (see [[n2a1-roadmap]] wave 1 task 2).
 */
export const COMMUNITY_HULLS_ATTRIBUTE = "communityHulls";

/**
 * Deterministic descriptor for a background community hull. Communities are
 * rendered as non-clickable background regions behind their member nodes,
 * not as selectable graph nodes.
 *
 * - `id` — the stable `community_id` (server sha256-of-members hash) so the
 *   hull identity survives snapshot re-renders.
 * - `label` — the server-supplied label from a legacy `kind: "community"`
 *   snapshot entry, or the id itself when no legacy entry exists.
 * - `keywords` — topical keywords from a legacy community entry, when present.
 * - `color` — the same `colorForCommunity` hue used for member symbols so the
 *   hull background and the member dots read as one cluster.
 * - `memberCount` — server-reported member count from a legacy community entry
 *   when present, otherwise the count of visible nodes carrying this
 *   `community_id`.
 * - `memberIds` — visible graph node ids carrying this `community_id`, used by
 *   the canvas to derive a screen-space hull from member positions without
 *   turning the community itself into a selectable Sigma node.
 * - `seed` — deterministic hash of the id, usable by the canvas as a PRNG
 *   seed for hull bounds / opacity jitter without a separate RNG.
 */
export interface CommunityHull {
  id: string;
  label: string;
  keywords: string[] | undefined;
  color: string;
  memberCount: number;
  memberIds: string[];
  seed: number;
}

/**
 * Derive community hull metadata from a snapshot payload.
 *
 * Visible file/folder/symbol nodes with a `community_id` contribute one hull
 * per distinct community id. Legacy `kind: "community"` snapshot entries
 * (which the adapter no longer turns into visible nodes) contribute their
 * label, keywords, and server-reported `member_count` when available so the
 * hull carries richer information than the member count alone.
 *
 * The hull color is always {@link colorForCommunity} so member symbols and the
 * hull background share the same hue.
 */
export function deriveCommunityHulls(
  snapshot: SnapshotPayload,
): CommunityHull[] {
  const visibleMembers = new Map<
    string,
    { ids: Set<string>; legacy?: SnapshotNode; workspaces: Map<string, number> }
  >();

  for (const node of snapshot.nodes) {
    if (node.kind === "community") {
      // Legacy community node: fold its metadata into the hull for this id.
      const cid = node.community_id ?? node.id;
      const entry = visibleMembers.get(cid);
      if (entry) {
        // Prefer the first legacy entry encountered; do not overwrite.
        if (!entry.legacy) entry.legacy = node;
      } else {
        visibleMembers.set(cid, {
          ids: new Set(),
          legacy: node,
          workspaces: new Map(),
        });
      }
      continue;
    }
    if (!node.community_id) continue;
    const cid = node.community_id;
    let entry = visibleMembers.get(cid);
    if (!entry) {
      entry = { ids: new Set(), workspaces: new Map() };
      visibleMembers.set(cid, entry);
    }
    entry.ids.add(node.id);
    if (node.workspace) {
      entry.workspaces.set(
        node.workspace,
        (entry.workspaces.get(node.workspace) ?? 0) + 1,
      );
    }
  }

  const hulls: CommunityHull[] = [];
  for (const [cid, entry] of visibleMembers) {
    const legacy = entry.legacy;
    // Match the hull background to the crate its members are colored by,
    // so the region and the dots inside it read as one crate. Falls back
    // to the per-community hue when no member carries a workspace tag.
    const dominantWorkspace = dominantKey(entry.workspaces);
    hulls.push({
      id: cid,
      label: legacy?.label ?? cid,
      keywords: legacy ? normalizeKeywords(legacy.keywords) : undefined,
      color: dominantWorkspace
        ? colorForWorkspace(dominantWorkspace)
        : colorForCommunity(cid),
      memberCount: legacy?.member_count ?? entry.ids.size,
      memberIds: Array.from(entry.ids).sort((a, b) => a.localeCompare(b, "en")),
      seed: fnv1a(cid),
    });
  }

  hulls.sort((a, b) => a.id.localeCompare(b.id, "en"));
  return hulls;
}

/**
 * Minimum member count for a community to be worth a background hull.
 * Tiny clusters add a glowing blob each without conveying structure —
 * dropping them is a lever against the "rainbow of balls" mush.
 */
export const MIN_HULL_MEMBERS = 6;

/**
 * Hard cap on rendered hulls. Even after the size filter, dozens of
 * overlapping translucent regions read as noise; we keep the largest
 * communities (the ones that actually shape the layout) and let the
 * smaller ones live as bare crate-colored dots.
 */
export const MAX_HULL_REGIONS = 16;

/**
 * Pick the hulls worth drawing: drop communities below
 * {@link MIN_HULL_MEMBERS} members, then keep the {@link MAX_HULL_REGIONS}
 * largest by member count. Ties broken by id so the selection is stable
 * across renders. Pure + exported so the render path stays thin and the
 * policy is unit-testable.
 */
export function selectRenderableHulls(hulls: CommunityHull[]): CommunityHull[] {
  return hulls
    .filter((h) => h.memberCount >= MIN_HULL_MEMBERS)
    .sort((a, b) =>
      b.memberCount !== a.memberCount
        ? b.memberCount - a.memberCount
        : a.id.localeCompare(b.id, "en"),
    )
    .slice(0, MAX_HULL_REGIONS);
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
 * Color routing. Every node is colored by a single **grouping key**
 * ({@link colorGroupKey}) that prefers the most specific signal available —
 * the crate derived from the path, else the detected community, else the
 * parent folder. Same group → same hue, regardless of node kind, so the
 * canvas reads as ~20 named crate regions instead of one flat color (the old
 * coarse-`workspace` behavior) or a per-community hash rainbow.
 *
 * The project root keeps a fixed apex accent; nodes with no usable key fall
 * back to a neutral slate.
 */
export function colorForNode(node: SnapshotNode): string {
  // Project root keeps its apex accent no matter what its slug hashes to.
  if (
    node.kind === "folder" &&
    (/project/i.test(node.label) || node.label === "" || !node.file_path)
  ) {
    return PROJECT_COLOR;
  }
  // Legacy community entries (not rendered) keep their own hue.
  if (node.kind === "community") {
    return colorForGroup(node.community_id || node.label || node.id);
  }
  const key = colorGroupKey(node);
  if (key) return colorForGroup(key);
  return node.kind === "symbol" ? SYMBOL_FALLBACK : PROJECT_COLOR;
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

/**
 * Edge kinds that express *containment* — structural nesting between
 * a container (file / type) and its contained definition / member.
 * These are never drawn as Sigma edges and never traversed by the
 * reducer / DOI frontier. Instead the adapter converts them into
 * `parentId` nesting metadata on the child node so the rendering layer
 * can position symbols inside their containing region.
 *
 * Exported so the reducers, DOI traversal, and store toggle guard can
 * reuse the same policy instead of each re-deriving the set.
 */
export const CONTAINMENT_EDGE_KINDS = new Set([
  "ContainsDefinition",
  "DeclaredInFile",
  "MemberOf",
]);

/** True when `kind` is a containment edge (never drawn / traversed). */
export function isContainmentEdgeKind(kind: string): boolean {
  return CONTAINMENT_EDGE_KINDS.has(kind);
}

function containmentEndpoints(edge: SnapshotEdge): {
  parentId: string;
  childId: string;
} | null {
  if (edge.kind === "ContainsDefinition") {
    return { parentId: edge.from, childId: edge.to };
  }
  if (edge.kind === "DeclaredInFile" || edge.kind === "MemberOf") {
    return { parentId: edge.to, childId: edge.from };
  }
  return null;
}

/** Base size scales with graph density — denser graphs get thinner strokes. */
function edgeBaseSize(nodeCount: number): number {
  if (nodeCount > 20000) return 0.4;
  if (nodeCount > 5000) return 0.6;
  return 1.0;
}

/**
 * Ceiling on edges handed to Sigma. A 10k-node "All" snapshot drags in
 * ~98k edges, and every one is a curved WebGL geometry — that volume is
 * the dominant cost of the initial-load hang and renders as an
 * undifferentiated wash anyway. Above this ceiling we keep only the
 * highest-salience edges (see {@link edgeSalience}); the rest are
 * dropped before they ever reach graphology/Sigma. Focused sub-views
 * (single workspace, DOI expansion) sit far below the cap and are
 * unaffected.
 */
export const MAX_RENDERED_EDGES = 12_000;

/**
 * Above this many rendered edges, intra-crate edges switch from curved
 * beziers to straight lines. Curved edges tessellate a quadratic curve
 * into many GPU segments; at a few thousand edges that geometry is the
 * bulk of the per-frame cost and the curvature is imperceptible in the
 * dense wash anyway. The few cross-crate edges stay curved + dashed so
 * they remain legible as the highlighted inter-module links. Focused
 * sub-views (single crate, DOI expansion) sit below the threshold and
 * keep the prettier curved rendering.
 */
export const STRAIGHT_EDGE_THRESHOLD = 2_500;

/** Sigma edge-program type for straight (rectangle) edges. */
export const STRAIGHT_EDGE_TYPE = "straight";
/** Sigma edge-program type for curved (bezier) edges. */
export const CURVED_EDGE_TYPE = "curved";

/**
 * Graph-level attribute carrying `{ rendered, total }` edge counts so the
 * canvas can surface "N of M edges" when the salience cap trims the set.
 * `total` counts drawable edges (post containment/self-loop/drop filters),
 * matching what `rendered` is selected from.
 */
export const EDGE_RENDER_STATS_ATTRIBUTE = "edgeRenderStats";

export interface EdgeRenderStats {
  rendered: number;
  total: number;
}

/** Read the `{ rendered, total }` edge counts stashed on a built graph. */
export function readEdgeRenderStats(graph: Graph): EdgeRenderStats | null {
  const stats = graph.getAttribute(EDGE_RENDER_STATS_ATTRIBUTE);
  if (
    stats &&
    typeof stats === "object" &&
    typeof (stats as EdgeRenderStats).rendered === "number" &&
    typeof (stats as EdgeRenderStats).total === "number"
  ) {
    return stats as EdgeRenderStats;
  }
  return null;
}

/**
 * Visual weight of a drawable edge — reuses the same factors that drive
 * stroke `size`, so the edges that read most strongly on the canvas are
 * exactly the ones the salience cap keeps: per-kind importance
 * (structural/OOP spine over the call graph), per-edge confidence, and a
 * cross-workspace boost.
 */
function edgeSalience(
  multiplier: number,
  confidence: number,
  isCrossWorkspace: boolean,
): number {
  const confidenceFactor = 0.4 + confidence * 0.6;
  return multiplier * confidenceFactor * (isCrossWorkspace ? 2.4 : 1);
}

// ── Adapter ─────────────────────────────────────────────────────────────────

export interface BuildGraphOptions {
  /** Drop self-loops? Default `true`. */
  dropSelfLoops?: boolean;
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
 * Communities are no longer added as visible graph nodes. Snapshot entries
 * with `kind: "community"` are skipped during node creation and their
 * metadata (label, keywords, member count) is folded into background
 * {@link CommunityHull} descriptors stored under the
 * {@link COMMUNITY_HULLS_ATTRIBUTE} graph attribute. Visible symbol/file
 * nodes that carry a `community_id` also contribute a hull so the canvas can
 * always draw a background region per community even without a legacy
 * community snapshot entry.
 *
 * Layout seeding strategy:
 *  - **Precomputed**: when every *visible* node carries a finite `x`/`y`
 *    (see `hasPrecomputedCoordinates`), positions are used verbatim, the
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
  const graph = new Graph({ multi: true, type: "directed" });

  // Communities are background hull metadata, not visible graph nodes.
  // Legacy `kind: "community"` entries are filtered out of the node set
  // here; their label/keywords/member_count are preserved via hull
  // derivation below. This keeps the adapter wire-compatible — the
  // server may still ship `kind: "community"` entries in old snapshots —
  // while ensuring no community node ever becomes a clickable Sigma node.
  const visibleNodes = snapshot.nodes.filter((n) => n.kind !== "community");

  // Derive hull metadata from visible members + legacy community entries
  // and stash it on the graph so the canvas can render background regions.
  const hulls = deriveCommunityHulls(snapshot);
  if (hulls.length > 0) {
    graph.setAttribute(COMMUNITY_HULLS_ATTRIBUTE, hulls);
  }

  const nodes = visibleNodes;
  const nodeCount = nodes.length;
  const ranks = nodes.map((n) => n.pagerank);
  const maxRank = ranks.length > 0 ? Math.max(...ranks, 0.000_001) : 1;

  // Precomputed branch — when every *visible* node ships a finite (x, y),
  // the server has already done the warm-time layout work and FA2 has
  // nothing to converge on. Use the coordinates verbatim, mark the
  // graph so `useSigmaGraph` can skip the FA2 supervisor, and exit
  // before the structural / golden-angle / community / random seed
  // branch below. The seed path is intentionally untouched for the
  // fallback case so existing 4xx / 5xx / pre-warm snapshots render
  // exactly as before.
  //
  // Only visible (non-community) nodes are checked for coordinates:
  // legacy community entries have no positions and are invisible anyway.
  if (
    nodeCount > 0 &&
    hasPrecomputedCoordinates({ ...snapshot, nodes: visibleNodes })
  ) {
    for (const node of nodes) {
      // hasPrecomputedCoordinates proved node.x / node.y are finite
      // numbers, so the non-null assertions are safe at runtime.
      addNode(graph, node, { x: node.x!, y: node.y! }, maxRank, nodeCount);
    }
    addEdgesFromSnapshot(graph, snapshot, nodeCount, {
      dropSelfLoops,
    });
    applyContainmentNesting(graph, snapshot);
    stampFileComplexity(graph);
    graph.setAttribute(PRECOMPUTED_LAYOUT_ATTRIBUTE, true);
    return graph;
  }

  // Deterministic layout branch: delegate to the pure layout module.
  // Seed with computeForceLayout so the adapter stays jitter-free;
  // the FA2 supervisor in useSigmaGraph runs on top.
  const layoutPositions = computeForceLayout({
    ...snapshot,
    nodes: visibleNodes,
  });
  for (const node of nodes) {
    const pos = layoutPositions.get(node.id) ?? { x: 0, y: 0 };
    addNode(graph, node, pos, maxRank, nodeCount);
  }

  // Edges — per-kind colors, base scaled by graph density, modulated
  // by per-edge confidence so hand-resolved edges trail brighter than
  // weak heuristic ones. Extracted so the precomputed-coords branch
  // above reuses the same edge-rendering pipeline. Containment edges
  // (ContainsDefinition / DeclaredInFile / MemberOf) are excluded here
  // and converted into nesting metadata by `applyContainmentNesting`.
  addEdgesFromSnapshot(graph, snapshot, nodeCount, {
    dropSelfLoops,
  });
  applyContainmentNesting(graph, snapshot);
  stampFileComplexity(graph);

  return graph;
}

/**
 * Give file nodes a `cognitive` value so the complexity heatmap works in the
 * Architecture (Files) lens, not just Calls (Functions). A file's complexity
 * is its **worst function** — the max `cognitive` over the symbol nodes that
 * share its path. Max (not sum) keeps the value on the same scale as function
 * cognitive, so it reads as a percentile against the same thresholds (which
 * are sampled from symbol nodes only — see `useGraphReducers`). Files with no
 * complexity-bearing symbols are left without a `cognitive` attribute.
 */
function stampFileComplexity(graph: Graph): void {
  const maxByPath = new Map<string, number>();
  graph.forEachNode((_id, attrs) => {
    if (attrs.kind !== "symbol") return;
    const cog = attrs.cognitive;
    const path = attrs.filePath;
    if (typeof cog !== "number" || !Number.isFinite(cog)) return;
    if (typeof path !== "string" || path.length === 0) return;
    const prev = maxByPath.get(path);
    if (prev === undefined || cog > prev) maxByPath.set(path, cog);
  });
  if (maxByPath.size === 0) return;
  graph.forEachNode((id, attrs) => {
    if (attrs.kind !== "file") return;
    const path =
      typeof attrs.filePath === "string" ? attrs.filePath : undefined;
    if (!path) return;
    const max = maxByPath.get(path);
    if (max !== undefined) graph.setNodeAttribute(id, "cognitive", max);
  });
}

function addEdgesFromSnapshot(
  graph: Graph,
  snapshot: SnapshotPayload,
  nodeCount: number,
  options: { dropSelfLoops: boolean },
): void {
  const { dropSelfLoops } = options;
  const nodeMap = new Map(snapshot.nodes.map((n) => [n.id, n]));
  const baseSize = edgeBaseSize(nodeCount);

  // Collect drawable edges with their fully-resolved attributes and a
  // salience score, then (if over the ceiling) keep only the strongest
  // before touching graphology. Dropping at this layer keeps the trimmed
  // edges out of Sigma's WebGL buffers entirely — the real load win.
  interface DrawableEdge {
    from: string;
    to: string;
    isCrossWorkspace: boolean;
    salience: number;
    attrs: Record<string, unknown>;
  }
  const drawable: DrawableEdge[] = [];

  for (const edge of snapshot.edges) {
    // Containment edges (ContainsDefinition / DeclaredInFile /
    // MemberOf) are structural nesting metadata, not drawn or
    // traversable edges. They are converted into `parentId` /
    // `childIds` nesting metadata by `applyContainmentNesting`.
    if (isContainmentEdgeKind(edge.kind)) continue;
    if (!graph.hasNode(edge.from) || !graph.hasNode(edge.to)) continue;
    if (dropSelfLoops && edge.from === edge.to) continue;
    const style = edgeStyleFor(edge.kind);
    if (style.drop) continue;
    const confidenceFactor = 0.4 + edge.confidence * 0.6;
    const sourceWorkspace = nodeMap.get(edge.from)?.workspace;
    const targetWorkspace = nodeMap.get(edge.to)?.workspace;
    const isCrossWorkspace =
      !!sourceWorkspace &&
      !!targetWorkspace &&
      sourceWorkspace !== targetWorkspace;
    const crossWorkspaceColor = isCrossWorkspace ? "#facc15" : undefined;
    drawable.push({
      from: edge.from,
      to: edge.to,
      isCrossWorkspace,
      salience: edgeSalience(
        style.sizeMultiplier,
        edge.confidence,
        isCrossWorkspace,
      ),
      attrs: {
        kind: edge.kind,
        confidence: edge.confidence,
        reason: edge.reason,
        sourceWorkspace,
        targetWorkspace,
        isCrossWorkspace,
        crossWorkspace: isCrossWorkspace,
        size: baseSize * style.sizeMultiplier * confidenceFactor *
          (isCrossWorkspace ? 2.4 : 1),
        color: crossWorkspaceColor ?? style.color,
        zIndex: isCrossWorkspace ? 20 : 1,
        lineStyle: isCrossWorkspace ? "dashed" : "solid",
      },
    });
  }

  const total = drawable.length;
  if (total > MAX_RENDERED_EDGES) {
    // Highest salience first; the slice keeps the strongest MAX edges.
    drawable.sort((a, b) => b.salience - a.salience);
    drawable.length = MAX_RENDERED_EDGES;
  }

  // Decide the edge program from the *rendered* count: dense graphs draw
  // intra-crate edges as cheap straight lines, sparse ones keep curves.
  const dense = drawable.length > STRAIGHT_EDGE_THRESHOLD;
  for (const e of drawable) {
    const curved = e.isCrossWorkspace || !dense;
    graph.addEdge(e.from, e.to, {
      ...e.attrs,
      type: curved ? CURVED_EDGE_TYPE : STRAIGHT_EDGE_TYPE,
      // curvature only applies to the bezier program; straight edges omit it.
      ...(curved
        ? { curvature: (e.isCrossWorkspace ? 0.28 : 0.12) + 0.04 }
        : {}),
    });
  }

  graph.setAttribute(EDGE_RENDER_STATS_ATTRIBUTE, {
    rendered: drawable.length,
    total,
  } satisfies EdgeRenderStats);
}

/**
 * Convert raw containment edges (`ContainsDefinition`, `DeclaredInFile`,
 * `MemberOf`) into nesting metadata on graphology nodes. For each
 * containment edge the adapter sets `parentId` on the child node and
 * accumulates `childIds` on the parent. The rendering layer (LOD /
 * hull canvas) uses this to draw symbols inside their containing file
 * or type region instead of drawing containment edges.
 *
 * Edge direction follows the server convention: `ContainsDefinition`
 * points parent → child, while `DeclaredInFile` / `MemberOf` point
 * child → parent. Both endpoints must already exist as graphology
 * nodes — containment edges to absent nodes are silently skipped.
 */
function applyContainmentNesting(
  graph: Graph,
  snapshot: SnapshotPayload,
): void {
  for (const edge of snapshot.edges) {
    const endpoints = containmentEndpoints(edge);
    if (endpoints === null) continue;
    const { parentId, childId } = endpoints;
    if (!graph.hasNode(parentId) || !graph.hasNode(childId)) continue;
    // The child may already carry a parentId from a higher-specificity
    // containment edge (e.g. MemberOf over ContainsDefinition). We keep
    // the first one encountered so the nesting tree stays deterministic.
    const existingParent = graph.getNodeAttribute(childId, "parentId");
    if (typeof existingParent === "string" && existingParent.length > 0) {
      continue;
    }
    graph.setNodeAttribute(childId, "parentId", parentId);
    const children = graph.getNodeAttribute(parentId, "childIds");
    if (Array.isArray(children)) {
      children.push(childId);
      graph.setNodeAttribute(parentId, "childIds", children);
    } else {
      graph.setNodeAttribute(parentId, "childIds", [childId]);
    }
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
  const workspaceColor = node.workspace
    ? colorForWorkspace(node.workspace)
    : undefined;
  const primaryLabel =
    node.workspace && node.workspace_context === true
      ? `${node.label} · ${node.workspace}`
      : node.label;
  graph.addNode(node.id, {
    label: primaryLabel,
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
    /**
     * The crate/community grouping key this node is colored by — stashed so
     * the crate legend can tally groups and the crate filter can isolate one
     * without re-deriving the key from the path on every reducer call.
     */
    colorGroup: colorGroupKey(node),
    communityId: node.community_id,
    workspaceKind: node.workspace_kind,
    workspace: node.workspace,
    workspaceColor,
    workspaceBadge: node.workspace ? workspaceBadge(node.workspace) : undefined,
    borderColor: workspaceColor,
    borderSize: workspaceColor
      ? node.workspace_context === true
        ? 1
        : 1.5
      : undefined,
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

// ── LOD tiers (continuous semantic zoom) ──────────────────────────────────

/**
 * Three LOD tiers driven by Sigma camera zoom ratio (continuous, no
 * mode toggle). Each tier controls which node kinds are visible and
 * what detail level is rendered:
 *
 *   far  — structural skeleton: crates / folders / files + community
 *          hull backgrounds. All symbol nodes hidden.
 *   mid  — structural + top-level symbols (class, struct, interface,
 *          trait, enum). Low-priority symbols hidden.
 *   close — everything visible including members, labels, and details.
 *
 * Transitions between tiers are approximately 200ms (the canvas
 * applies a CSS transition on the Sigma container; the discrete
 * tier change itself is instant but the camera zoom animation that
 * triggers it typically lasts 200–400ms).
 */
export type LodTier = "far" | "mid" | "close";

/**
 * Sigma camera ratio thresholds for LOD tier boundaries.
 *
 * Sigma's `getCamera().ratio` semantics:
 *   - ratio > 1 → zoomed out (far view)
 *   - ratio = 1 → fit-to-view
 *   - ratio < 1 → zoomed in (close view)
 */
export const LOD_FAR_RATIO = 2.0;
export const LOD_MID_RATIO = 0.4;

/**
 * Derive the LOD tier from the current Sigma camera zoom ratio.
 * Pure, stateless, O(1) — called on every `afterRender` frame.
 */
export function lodTierForZoom(cameraRatio: number): LodTier {
  if (!Number.isFinite(cameraRatio)) return "close";
  if (cameraRatio >= LOD_FAR_RATIO) return "far";
  if (cameraRatio >= LOD_MID_RATIO) return "mid";
  return "close";
}

/**
 * Symbol kinds considered "low-priority" at mid LOD — they are hidden
 * when the camera is between mid and far thresholds so the canvas
 * stays legible. At close LOD they become visible again.
 */
const MID_TIER_HIDDEN_SYMBOL_KINDS = new Set([
  "variable",
  "const",
  "static",
  "property",
  "field",
  "import",
  "other",
]);

/**
 * Should a symbol node with the given `symbol_kind` be visible at
 * the mid LOD tier? Returns `true` for structural / top-level symbols
 * (class, struct, interface, trait, enum, function, method,
 * constructor, impl, type) and `false` for low-priority symbols.
 */
export function isSymbolVisibleAtMidTier(
  symbolKind: string | undefined,
): boolean {
  if (!symbolKind) return true;
  return !MID_TIER_HIDDEN_SYMBOL_KINDS.has(symbolKind);
}

// ── Viewport culling ──────────────────────────────────────────────────────

/**
 * Viewport rectangle in graph-coordinate space. Computed from the
 * Sigma camera state + container dimensions on every `afterRender`
 * frame. Used by the nodeReducer for viewport culling on large
 * graphs (>~8000 nodes).
 */
export interface ViewportBounds {
  minX: number;
  minY: number;
  maxX: number;
  maxY: number;
}

/**
 * Value-equality for viewport bounds. `sigma.getViewportBounds()` returns a
 * fresh object on every call, so an identity (`!==`) comparison always reports
 * a change. The reducer's `afterRender` handler uses that comparison to decide
 * whether to `refresh()`; with identity comparison it refreshes every frame,
 * and since `refresh()` schedules another render, that's a perpetual 60fps
 * render loop (re-running every reducer each frame, even at idle) on graphs
 * past `VIEWPORT_CULLING_THRESHOLD`. Comparing by value breaks the loop.
 *
 * Two `null`s are equal (culling inactive on both sides); a `null`/object
 * mismatch is a change.
 */
export function viewportBoundsEqual(
  a: ViewportBounds | null,
  b: ViewportBounds | null,
): boolean {
  if (a === b) return true;
  if (a === null || b === null) return false;
  return (
    a.minX === b.minX &&
    a.minY === b.minY &&
    a.maxX === b.maxX &&
    a.maxY === b.maxY
  );
}

/**
 * Node-count threshold above which viewport culling activates.
 * Below this, all nodes are considered in-viewport (no culling).
 */
export const VIEWPORT_CULLING_THRESHOLD = 8_000;

/**
 * Margin (in graph-coordinate units) added around the viewport
 * bounds for culling. Nodes within this margin are kept visible
 * even if they're just outside the viewport, so pan/scroll feels
 * smooth without popping.
 */
export const VIEWPORT_CULLING_MARGIN = 200;

/**
 * Check whether a node at (x, y) falls inside the viewport bounds
 * (with margin). Returns `true` if the node should be visible.
 */
export function isInViewport(
  x: number,
  y: number,
  bounds: ViewportBounds,
): boolean {
  const m = VIEWPORT_CULLING_MARGIN;
  return (
    x >= bounds.minX - m &&
    x <= bounds.maxX + m &&
    y >= bounds.minY - m &&
    y <= bounds.maxY + m
  );
}

// ── Double-click helper ───────────────────────────────────────────────────

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
