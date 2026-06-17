import type Graph from "graphology";

export interface PerCommunityAttractionNodeMetadata {
  /**
   * Stable community id from memoryCommunityClustering. Nodes without a
   * non-empty id are treated as unclustered and remain in the normal FA2 flow.
   */
  communityId?: string | null;
  /** Label shape produced by memoryCommunityClustering. */
  label?: string | null;
  /** Alias for callers that store the label directly on graph node attributes. */
  communityLabel?: string | null;
}

export type PerCommunityAttractionMetadata =
  | ReadonlyMap<string, PerCommunityAttractionNodeMetadata | null | undefined>
  | Readonly<
      Record<string, PerCommunityAttractionNodeMetadata | null | undefined>
    >;

export interface PerCommunityAttractionOptions {
  /** Radius of the deterministic circle on which community centers are placed. */
  clusterRadius?: number;
  /** Fraction of remaining distance to pull per iteration. Clamped to [0, 1]. */
  strength?: number;
  /**
   * Number of bounded attraction iterations. Negative/non-finite values become 0.
   */
  iterations?: number;
  /**
   * Minimum member count required before a community receives attraction; clamped
   * to at least 2 so singleton nodes stay in the FA2 flow.
   */
  minCommunitySize?: number;
}

export interface PerCommunityCenter {
  x: number;
  y: number;
}

export interface PerCommunityAttractionResult {
  /** Center assigned to each attraction-eligible community id. */
  centers: Map<string, PerCommunityCenter>;
  /** Deterministic lexicographic center assignment order. */
  orderedCommunityIds: string[];
  /** Nodes with attraction-eligible metadata after singleton filtering. */
  eligibleNodeCount: number;
  /** Eligible nodes whose x/y position changed. */
  movedNodeCount: number;
  iterations: number;
  strength: number;
}

export const DEFAULT_PER_COMMUNITY_ATTRACTION_OPTIONS = {
  clusterRadius: 600,
  strength: 0.08,
  iterations: 24,
  minCommunitySize: 2,
} as const satisfies Required<PerCommunityAttractionOptions>;

type NodePosition = {
  nodeId: string;
  x: number;
  y: number;
};

/**
 * Pulls already-clustered graphology nodes toward deterministic per-community
 * centers without touching unclustered/singleton nodes.
 *
 * This helper is intentionally pure with respect to React/Sigma and only mutates
 * the graphology graph's `x`/`y` node attributes. Community ids are sorted before
 * assigning centers on a circle, so a fixed community-id set always receives the
 * same centers. Singleton communities are ignored by default because
 * memoryCommunityClustering collapses singleton/no-intra-edge communities into
 * the unclustered/no-attraction bucket.
 */
export function applyPerCommunityAttraction(
  graph: Graph,
  metadataByNode: PerCommunityAttractionMetadata,
  options: PerCommunityAttractionOptions = {},
): PerCommunityAttractionResult {
  const clusterRadius = finiteOrDefault(
    options.clusterRadius,
    DEFAULT_PER_COMMUNITY_ATTRACTION_OPTIONS.clusterRadius,
  );
  const strength = clamp01(
    finiteOrDefault(
      options.strength,
      DEFAULT_PER_COMMUNITY_ATTRACTION_OPTIONS.strength,
    ),
  );
  const iterations = Math.max(
    0,
    Math.floor(
      finiteOrDefault(
        options.iterations,
        DEFAULT_PER_COMMUNITY_ATTRACTION_OPTIONS.iterations,
      ),
    ),
  );
  const minCommunitySize = Math.max(
    2,
    Math.floor(
      finiteOrDefault(
        options.minCommunitySize,
        DEFAULT_PER_COMMUNITY_ATTRACTION_OPTIONS.minCommunitySize,
      ),
    ),
  );

  const grouped = new Map<string, NodePosition[]>();

  graph.forEachNode((nodeId, attributes) => {
    const metadata = getMetadata(metadataByNode, nodeId);
    const communityId = normalizeCommunityId(metadata?.communityId);
    if (!communityId) return;

    const nodes = grouped.get(communityId) ?? [];
    nodes.push({
      nodeId,
      x: finiteOrDefault(attributes.x, 0),
      y: finiteOrDefault(attributes.y, 0),
    });
    grouped.set(communityId, nodes);
  });

  const orderedCommunityIds = [...grouped.entries()]
    .filter(([, nodes]) => nodes.length >= minCommunitySize)
    .map(([communityId]) => communityId)
    .sort(compareCommunityIds);

  const centers = assignCommunityCenters(orderedCommunityIds, clusterRadius);

  let eligibleNodeCount = 0;
  let movedNodeCount = 0;

  for (const communityId of orderedCommunityIds) {
    const center = centers.get(communityId);
    const nodes = grouped.get(communityId);
    if (!center || !nodes) continue;

    for (const node of nodes) {
      eligibleNodeCount += 1;
      const next = pullTowardCenter(node, center, strength, iterations);
      if (next.x !== node.x || next.y !== node.y) {
        movedNodeCount += 1;
        graph.setNodeAttribute(node.nodeId, "x", next.x);
        graph.setNodeAttribute(node.nodeId, "y", next.y);
      }
    }
  }

  return {
    centers,
    orderedCommunityIds,
    eligibleNodeCount,
    movedNodeCount,
    iterations,
    strength,
  };
}

export function assignCommunityCenters(
  orderedCommunityIds: readonly string[],
  clusterRadius: number = DEFAULT_PER_COMMUNITY_ATTRACTION_OPTIONS.clusterRadius,
): Map<string, PerCommunityCenter> {
  const radius = finiteOrDefault(
    clusterRadius,
    DEFAULT_PER_COMMUNITY_ATTRACTION_OPTIONS.clusterRadius,
  );
  const centers = new Map<string, PerCommunityCenter>();
  const count = orderedCommunityIds.length;
  if (count === 0) return centers;

  orderedCommunityIds.forEach((communityId, index) => {
    const theta = (2 * Math.PI * index) / count;
    centers.set(communityId, {
      x: Math.cos(theta) * radius,
      y: Math.sin(theta) * radius,
    });
  });

  return centers;
}

function pullTowardCenter(
  node: NodePosition,
  center: PerCommunityCenter,
  strength: number,
  iterations: number,
): PerCommunityCenter {
  let x = node.x;
  let y = node.y;

  for (let i = 0; i < iterations; i += 1) {
    x += (center.x - x) * strength;
    y += (center.y - y) * strength;
  }

  return { x, y };
}

function isMetadataMap(
  metadataByNode: PerCommunityAttractionMetadata,
): metadataByNode is ReadonlyMap<
  string,
  PerCommunityAttractionNodeMetadata | null | undefined
> {
  return metadataByNode instanceof Map;
}

function getMetadata(
  metadataByNode: PerCommunityAttractionMetadata,
  nodeId: string,
): PerCommunityAttractionNodeMetadata | null | undefined {
  if (isMetadataMap(metadataByNode)) {
    return metadataByNode.get(nodeId);
  }
  return metadataByNode[nodeId];
}

function normalizeCommunityId(value: string | null | undefined): string | null {
  if (typeof value !== "string") return null;
  const trimmed = value.trim();
  return trimmed.length > 0 ? trimmed : null;
}

function finiteOrDefault(value: unknown, fallback: number): number {
  return typeof value === "number" && Number.isFinite(value) ? value : fallback;
}

function clamp01(value: number): number {
  return Math.min(1, Math.max(0, value));
}

function compareCommunityIds(left: string, right: string): number {
  if (left < right) return -1;
  if (left > right) return 1;
  return 0;
}
