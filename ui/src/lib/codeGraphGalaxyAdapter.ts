/**
 * codeGraphGalaxyAdapter — translate a `code_graph snapshot` payload into
 * the generic GalaxyData the 3D galaxy renderer consumes.
 *
 * Edge diet: containment spokes (ContainsDefinition) render as-is, but
 * usage edges (FileReference/SymbolReference/Reads/Writes/…) collapse to
 * ONE undirected file→file "import" edge per file pair — per-symbol-use
 * beams into mega-hubs (every crate → std::println!) otherwise bury the
 * galaxy in white trunks (~283k → ~72k edges on whole-repo djinn).
 * External symbols (no containing file in the repo) lose all their edges
 * in the collapse and are dropped entirely.
 *
 * Degree is computed from the collapsed edge list. Group is the
 * crate/top-level directory, parent is the containing file (from
 * ContainsDefinition). Positions come from `layoutGalaxy` until the
 * server ships 3D coordinates in the snapshot (proposal lmkv).
 *
 * `snapshotHotspots` derives the complexity/co-change leaderboard the
 * HotspotPanel renders: per-file worst cognitive complexity joined with
 * qoxm co-change coupling (partners, max score, recency).
 */

import { layoutGalaxy } from "@/components/galaxy/galaxyLayout";
import type { GalaxyData, GalaxyNode } from "@/components/galaxy/galaxyTypes";
import type { SnapshotPayload } from "@/lib/codeGraphAdapter";

const TYPE_LIKE_SYMBOL_KINDS = new Set([
  "class",
  "interface",
  "struct",
  "enum",
  "trait",
  "type",
]);

/** Sonar cognitive-complexity bands: below WARN reads green ("fine"),
 * WARN..SEVERE yellow ("needs attention" — Sonar's default per-function
 * threshold is 15), at or above SEVERE red ("very complex"). */
export const COMPLEXITY_WARN = 15;
export const COMPLEXITY_SEVERE = 25;

/** Usage kinds collapsed to one file→file edge per file pair. Everything
 * else (containment, Implements/Extends, process steps) renders raw. */
const USAGE_EDGE_KINDS = new Set([
  "FileReference",
  "SymbolReference",
  "Reads",
  "Writes",
  "TraitDispatchCall",
  "EntryPointOf",
]);

/** Pair-dedupe key separator: node ids can contain spaces, so the separator
 * must be a character that can never appear in an id. */
const PAIR_SEP = "\u0000";

/** File labels arrive as full repo paths — sprites want the basename. */
function displayLabel(label: string, kind: string): string {
  if (kind !== "file" && kind !== "folder") return label;
  const segments = label.split("/");
  return segments[segments.length - 1] || label;
}

/**
 * Crate-aware grouping: `server/crates/djinn-graph/...` → `djinn-graph`;
 * everything else groups by its first three path segments (finer than
 * two — more clusters spread the galaxy shell better on big graphs).
 */
export function deriveGalaxyGroup(
  filePath: string | undefined,
  workspace: string | undefined,
): string | undefined {
  if (!filePath) return workspace;
  const crateMatch = /(?:^|\/)crates\/([^/]+)/.exec(filePath);
  const prefix = workspace ? `${workspace}:` : "";
  if (crateMatch) return `${prefix}${crateMatch[1]}`;
  const segments = filePath.split("/").filter(Boolean);
  if (segments.length <= 1) return `${prefix}${segments[0] ?? "root"}`;
  return `${prefix}${segments.slice(0, Math.min(3, segments.length - 1) || 1).join("/")}`;
}

export interface SnapshotToGalaxyOptions {
  /**
   * Run the (synchronous, potentially seconds-long) force layout inline.
   * Default true — right for module-scope Storybook fixtures. The live
   * page passes false and runs `layoutGalaxy` in the worker instead.
   */
  layout?: boolean;
}

/** Deterministic layout seed for a project id (shared with the worker path). */
export function galaxyLayoutSeed(projectId: string | undefined): number {
  return hashSeed(projectId ?? "galaxy");
}

export function snapshotToGalaxy(
  snapshot: SnapshotPayload,
  options: SnapshotToGalaxyOptions = {},
): GalaxyData {
  // Containment parents. ContainsDefinition points container→member;
  // DeclaredInFile points member→file. DeclaredInFile duplicates
  // ContainsDefinition 1:1 in real snapshots, so it feeds the parent map
  // but is dropped from the render list (drawing both would double the
  // spoke brightness).
  const parentById = new Map<string, string>();
  for (const e of snapshot.edges) {
    if (e.kind === "ContainsDefinition") parentById.set(e.to, e.from);
    else if (e.kind === "DeclaredInFile") parentById.set(e.from, e.to);
  }

  // Symbols with no containing file are external (std, third-party
  // crates): the file-level collapse below removes every edge they had,
  // so keeping them would just scatter disconnected dust.
  const kept = snapshot.nodes.filter(
    (n) =>
      n.kind !== "community" &&
      !(n.kind === "symbol" && !parentById.has(n.id)),
  );
  const keptIds = new Set(kept.map((n) => n.id));
  const kindById = new Map(kept.map((n) => [n.id, n.kind]));

  /** File-level anchor: files/folders/processes are their own; symbols
   * resolve to their containing file. */
  const fileOf = (id: string): string | undefined =>
    kindById.get(id) === "symbol" ? parentById.get(id) : id;

  const edges: GalaxyData["edges"] = [];
  const seenPairs = new Set<string>();
  for (const e of snapshot.edges) {
    if (!keptIds.has(e.from) || !keptIds.has(e.to)) continue;
    if (e.kind === "DeclaredInFile") continue;
    if (USAGE_EDGE_KINDS.has(e.kind)) {
      const a = fileOf(e.from);
      const b = fileOf(e.to);
      // Intra-file uses are invisible at galaxy scale; unresolved ends
      // are external and already dropped from the node set.
      if (!a || !b || a === b) continue;
      const key = a < b ? `${a}${PAIR_SEP}${b}` : `${b}${PAIR_SEP}${a}`;
      if (seenPairs.has(key)) continue;
      seenPairs.add(key);
      edges.push({ source: a, target: b, kind: "FileReference" });
      continue;
    }
    edges.push({ source: e.from, target: e.to, kind: e.kind });
  }

  // Proposal lmkv: the server ships a warm-time galaxy layout + degree per node
  // in the snapshot. When EVERY rendered (kept) node carries finite server
  // coordinates, we render them directly and skip the client force layout;
  // otherwise (legacy blobs, the gitignored Storybook fixture) we fall back to
  // the worker/inline path below, exactly as before.
  const serverById = new Map<
    string,
    { gx: number; gy: number; gz: number }
  >();
  for (const n of snapshot.nodes) {
    if (!keptIds.has(n.id)) continue;
    if (
      typeof n.gx === "number" &&
      typeof n.gy === "number" &&
      typeof n.gz === "number"
    ) {
      serverById.set(n.id, { gx: n.gx, gy: n.gy, gz: n.gz });
    }
  }
  const serverPositioned = kept.length > 0 && serverById.size === kept.length;

  // Prefer the server-shipped degree; fall back to the collapsed-edge count.
  const degreeById = new Map<string, number>();
  for (const edge of edges) {
    degreeById.set(edge.source, (degreeById.get(edge.source) ?? 0) + 1);
    degreeById.set(edge.target, (degreeById.get(edge.target) ?? 0) + 1);
  }
  const serverDegreeById = new Map<string, number>();
  for (const n of snapshot.nodes) {
    if (keptIds.has(n.id) && typeof n.degree === "number") {
      serverDegreeById.set(n.id, n.degree);
    }
  }

  const nodes: GalaxyNode[] = kept.map((n) => {
    const degree = serverDegreeById.get(n.id) ?? degreeById.get(n.id) ?? 0;
    const server = serverById.get(n.id);
    const typeLike =
      n.symbol_kind !== undefined &&
      TYPE_LIKE_SYMBOL_KINDS.has(n.symbol_kind.toLowerCase());
    // Size scheme: Folder 12, File 8, type-like 6, other symbols 4 — plus
    // the degree boost for hubs (min(deg*0.3, 10) past 5).
    const baseSize =
      n.kind === "folder" ? 12 : n.kind === "file" ? 8 : typeLike ? 6 : 4;
    const degreeBoost = degree > 5 ? Math.min(degree * 0.3, 10) : 0;
    return {
      id: n.id,
      label: displayLabel(n.label, n.kind),
      x: server?.gx ?? 0,
      y: server?.gy ?? 0,
      z: server?.gz ?? 0,
      degree,
      size: baseSize + degreeBoost,
      group: deriveGalaxyGroup(n.file_path, n.workspace),
      parent: parentById.get(n.id),
      isTest: n.is_test === true,
      workspace: n.workspace,
    };
  });

  // Fast path: server already positioned every node — never run the layout,
  // regardless of the `layout` option. Otherwise honor the option (Storybook
  // fixtures pass the default `true`; GalaxyView passes `false` and runs the
  // worker itself).
  if (!serverPositioned && options.layout !== false) {
    layoutGalaxy(nodes, edges, galaxyLayoutSeed(snapshot.project_id));
  }

  // The "showing N of M" banner is about SERVER truncation — nodes and
  // edges we drop on purpose (external symbols, collapsed usage edges)
  // must not trip it.
  const serverTruncated =
    snapshot.total_nodes !== undefined &&
    snapshot.total_nodes > snapshot.nodes.length;
  return {
    nodes,
    edges,
    totalNodes: serverTruncated ? snapshot.total_nodes : undefined,
    totalEdges: serverTruncated ? snapshot.total_edges : undefined,
    serverPositioned,
  };
}

// ── Hotspots (complexity × co-change) ───────────────────────────────────────

/** One leaderboard row: a file's worst cognitive complexity joined with its
 * qoxm co-change coupling. `partnerIds` feeds the fly-to/highlight set. */
export interface GalaxyHotspot {
  fileId: string;
  /** Full repo path (rows show the basename, tooltip the full path). */
  path: string;
  workspace?: string;
  isTest: boolean;
  /** Max cognitive complexity among the file's scored functions. */
  complexity: number;
  /** Label of the worst-scoring function. */
  worstSymbol: string;
  /** Number of scored (function-like) symbols in the file. */
  functionCount: number;
  /** Co-change partner file ids (empty when uncoupled). */
  partnerIds: string[];
  /** Max co-change coupling score across partners (0..1; 0 = uncoupled). */
  coupling: number;
  /** Most recent co-change, as an epoch day (from the qoxm edge reason). */
  lastCoChangeDay?: number;
  /** Hotspot rank basis: complexity × (1 + Σ partner coupling scores) —
   * uncoupled files rank by complexity alone. */
  score: number;
}

/** Shape of the qoxm co-change edge reason: `cochange;last_day=<i64>`. */
const COCHANGE_REASON = /^cochange;last_day=(-?\d+)$/;

/**
 * Derive the hotspot leaderboard from a snapshot. Only files with at least
 * one scored function appear — co-change coupling without any complexity is
 * churn on simple code, not a hotspot. Sorted by `score`, descending.
 */
export function snapshotHotspots(snapshot: SnapshotPayload): GalaxyHotspot[] {
  const nodeById = new Map(snapshot.nodes.map((n) => [n.id, n]));

  // Same containment pass as snapshotToGalaxy — symbols → containing file.
  const parentById = new Map<string, string>();
  for (const e of snapshot.edges) {
    if (e.kind === "ContainsDefinition") parentById.set(e.to, e.from);
    else if (e.kind === "DeclaredInFile") parentById.set(e.from, e.to);
  }

  const complexityByFile = new Map<
    string,
    { complexity: number; worstSymbol: string; functionCount: number }
  >();
  for (const n of snapshot.nodes) {
    if (n.kind !== "symbol" || typeof n.cognitive !== "number") continue;
    const fileId = parentById.get(n.id);
    if (!fileId) continue;
    const entry = complexityByFile.get(fileId);
    if (!entry) {
      complexityByFile.set(fileId, {
        complexity: n.cognitive,
        worstSymbol: n.label,
        functionCount: 1,
      });
    } else {
      entry.functionCount += 1;
      if (n.cognitive > entry.complexity) {
        entry.complexity = n.cognitive;
        entry.worstSymbol = n.label;
      }
    }
  }

  const couplingByFile = new Map<
    string,
    { partnerIds: string[]; max: number; sum: number; lastDay?: number }
  >();
  for (const e of snapshot.edges) {
    if (e.kind !== "CoChangedWith") continue;
    const day = e.reason
      ? Number(COCHANGE_REASON.exec(e.reason)?.[1])
      : Number.NaN;
    for (const [self, partner] of [
      [e.from, e.to],
      [e.to, e.from],
    ]) {
      let entry = couplingByFile.get(self);
      if (!entry) couplingByFile.set(self, (entry = { partnerIds: [], max: 0, sum: 0 }));
      entry.partnerIds.push(partner);
      entry.max = Math.max(entry.max, e.confidence);
      entry.sum += e.confidence;
      if (Number.isFinite(day)) {
        entry.lastDay = entry.lastDay === undefined ? day : Math.max(entry.lastDay, day);
      }
    }
  }

  const hotspots: GalaxyHotspot[] = [];
  for (const [fileId, cx] of complexityByFile) {
    if (cx.complexity <= 0) continue;
    const node = nodeById.get(fileId);
    if (!node) continue;
    const coupling = couplingByFile.get(fileId);
    hotspots.push({
      fileId,
      path: node.file_path ?? node.label,
      workspace: node.workspace,
      isTest: node.is_test === true,
      complexity: cx.complexity,
      worstSymbol: cx.worstSymbol,
      functionCount: cx.functionCount,
      partnerIds: coupling?.partnerIds ?? [],
      coupling: coupling?.max ?? 0,
      lastCoChangeDay: coupling?.lastDay,
      score: cx.complexity * (1 + (coupling?.sum ?? 0)),
    });
  }
  return hotspots.sort(
    (a, b) => b.score - a.score || a.path.localeCompare(b.path),
  );
}

function hashSeed(input: string): number {
  let h = 2166136261;
  for (let i = 0; i < input.length; i++) {
    h ^= input.charCodeAt(i);
    h = Math.imul(h, 16777619);
  }
  return h >>> 0;
}
