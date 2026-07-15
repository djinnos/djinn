/**
 * codeGraphGalaxyAdapter — translate a `code_graph snapshot` payload into
 * the generic GalaxyData the 3D galaxy renderer consumes.
 *
 * Degree is computed from the edge list (all kinds — containment included,
 * matching how the stellar scale was tuned). Group is the crate/top-level
 * directory, parent is the containing file (from ContainsDefinition), and
 * heat is the node's cognitive-complexity percentile among function-like
 * nodes. Positions come from `layoutGalaxy` until the server ships 3D
 * coordinates in the snapshot (proposal lmkv).
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
  const kept = snapshot.nodes.filter((n) => n.kind !== "community");
  const keptIds = new Set(kept.map((n) => n.id));

  // Containment parents. ContainsDefinition points container→member;
  // DeclaredInFile points member→file. DeclaredInFile duplicates
  // ContainsDefinition 1:1 in real snapshots, so it feeds the parent map
  // but is dropped from the render list (drawing both would double the
  // spoke brightness).
  const parentById = new Map<string, string>();
  for (const e of snapshot.edges) {
    if (!keptIds.has(e.from) || !keptIds.has(e.to)) continue;
    if (e.kind === "ContainsDefinition") parentById.set(e.to, e.from);
    else if (e.kind === "DeclaredInFile") parentById.set(e.from, e.to);
  }

  const edges = snapshot.edges
    .filter(
      (e) =>
        keptIds.has(e.from) &&
        keptIds.has(e.to) &&
        e.kind !== "DeclaredInFile",
    )
    .map((e) => ({ source: e.from, target: e.to, kind: e.kind }));

  const degreeById = new Map<string, number>();
  for (const edge of edges) {
    degreeById.set(edge.source, (degreeById.get(edge.source) ?? 0) + 1);
    degreeById.set(edge.target, (degreeById.get(edge.target) ?? 0) + 1);
  }

  // Cognitive-complexity percentile → heat.
  const cognitives = kept
    .map((n) => n.cognitive)
    .filter((c): c is number => typeof c === "number")
    .sort((a, b) => a - b);
  const percentile = (value: number): number => {
    if (cognitives.length < 2) return 0.5;
    let lo = 0;
    let hi = cognitives.length;
    while (lo < hi) {
      const mid = (lo + hi) >> 1;
      if (cognitives[mid] <= value) lo = mid + 1;
      else hi = mid;
    }
    return lo / cognitives.length;
  };

  const nodes: GalaxyNode[] = kept.map((n) => {
    const degree = degreeById.get(n.id) ?? 0;
    const typeLike =
      n.symbol_kind !== undefined &&
      TYPE_LIKE_SYMBOL_KINDS.has(n.symbol_kind.toLowerCase());
    // Size scheme: Folder 12, File 8, type-like 6, other symbols 4 — plus
    // the degree boost for hubs (min(deg*0.3, 10) past 5).
    const baseSize =
      n.kind === "folder" ? 12 : n.kind === "file" ? 8 : typeLike ? 6 : 4;
    const degreeBoost = degree > 5 ? Math.min(degree * 0.3, 10) : 0;
    const hasCognitive = typeof n.cognitive === "number";
    return {
      id: n.id,
      label: displayLabel(n.label, n.kind),
      x: 0,
      y: 0,
      z: 0,
      degree,
      size: baseSize + degreeBoost,
      group: deriveGalaxyGroup(n.file_path, n.workspace),
      parent: parentById.get(n.id),
      heat: hasCognitive ? percentile(n.cognitive as number) : undefined,
      heatEligible: hasCognitive,
      isTest: n.is_test === true,
      workspace: n.workspace,
    };
  });

  if (options.layout !== false) {
    layoutGalaxy(nodes, edges, galaxyLayoutSeed(snapshot.project_id));
  }

  return {
    nodes,
    edges,
    totalNodes: snapshot.total_nodes,
    totalEdges: snapshot.total_edges,
  };
}

function hashSeed(input: string): number {
  let h = 2166136261;
  for (let i = 0; i < input.length; i++) {
    h ^= input.charCodeAt(i);
    h = Math.imul(h, 16777619);
  }
  return h >>> 0;
}
