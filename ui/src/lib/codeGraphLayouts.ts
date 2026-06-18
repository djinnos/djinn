import type Graph from "graphology";

import type {
  SnapshotEdge,
  SnapshotNode,
  SnapshotNodeKind,
  SnapshotPayload,
} from "@/lib/codeGraphAdapter";

export interface LayoutPosition {
  x: number;
  y: number;
}

export interface ForceLayoutOptions {
  structuralSpread?: number;
  childJitter?: number;
  clusterJitter?: number;
}

type LayoutInput = SnapshotPayload | Graph;

type LayoutNode = Pick<
  SnapshotNode,
  "id" | "kind" | "symbol_kind" | "pagerank" | "community_id"
>;

type LayoutEdge = Pick<SnapshotEdge, "from" | "to" | "kind">;

const GOLDEN_ANGLE = Math.PI * (3 - Math.sqrt(5));
const DEFAULT_LAYER_SPREAD = 180;
const DEFAULT_LAYER_GAP = 360;
const DEFAULT_RADIAL_BASE_RADIUS = 180;

const HIERARCHY_KINDS = new Set([
  "ContainsDefinition",
  "DeclaredInFile",
  "FileReference",
]);

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

const SYMBOL_SUB_BUCKET_PRIORITY = new Map<string, number>([
  ["function", 0],
  ["method", 0],
  ["constructor", 0],
  ["class", 1],
  ["struct", 1],
  ["interface", 1],
  ["trait", 1],
  ["enum", 1],
  ["impl", 1],
  ["field", 2],
  ["variable", 2],
  ["const", 2],
  ["static", 2],
  ["property", 2],
  ["import", 2],
  ["other", 2],
]);

/**
 * Deterministic seed positions for the force path. ForceAtlas2 can still run on
 * top of these positions, but this helper itself is pure and never calls
 * a nondeterministic random source.
 */
export function computeForceLayout(
  snapshot: LayoutInput,
  options: ForceLayoutOptions = {},
): Map<string, LayoutPosition> {
  const { nodes, edges, projectId } = normalizeLayoutInput(snapshot);
  const nodeCount = nodes.length;
  const structuralSpread =
    options.structuralSpread ?? Math.sqrt(Math.max(nodeCount, 1)) * 40;
  const childJitter =
    options.childJitter ?? Math.sqrt(Math.max(nodeCount, 1)) * 3;
  const clusterJitter =
    options.clusterJitter ?? Math.sqrt(Math.max(nodeCount, 1)) * 1.5;

  const nodeMap = new Map(nodes.map((node) => [node.id, node]));
  const childToParent = hierarchyParents(edges, nodeMap);
  const parentToChildren = new Map<string, string[]>();
  for (const [child, parent] of childToParent) {
    const children = parentToChildren.get(parent) ?? [];
    children.push(child);
    parentToChildren.set(parent, children);
  }
  for (const children of parentToChildren.values()) children.sort(compareIds);

  const structuralNodes = nodes.filter(
    (node) =>
      node.kind === "folder" || node.kind === "file" || node.kind === "community",
  );
  const structuralCount = Math.max(structuralNodes.length, 1);

  const communityIds = [
    ...new Set(
      nodes.flatMap((node) =>
        node.community_id ? [node.community_id] : [],
      ),
    ),
  ].sort(compareIds);
  const clusterCenters = new Map<string, LayoutPosition>();
  const clusterSpread = structuralSpread * 0.8;
  communityIds.forEach((communityId, index) => {
    clusterCenters.set(
      communityId,
      spiralPosition(index, communityIds.length, clusterSpread, 0),
    );
  });

  const positions = new Map<string, LayoutPosition>();

  structuralNodes.forEach((node, index) => {
    const base = spiralPosition(index, structuralCount, structuralSpread, 0);
    const jitter = structuralSpread * 0.15;
    const [jx, jy] = hashJitter(`${projectId}:${node.id}:structural`, jitter);
    positions.set(node.id, { x: base.x + jx, y: base.y + jy });
  });

  const placeNode = (id: string): void => {
    if (positions.has(id)) return;
    const node = nodeMap.get(id);
    if (!node) return;

    const communityId = node.community_id;
    const isClusterableSymbol =
      node.kind === "symbol" && SYMBOL_CLUSTER_KINDS.has(node.symbol_kind ?? "");
    const center = communityId ? clusterCenters.get(communityId) : undefined;

    if (isClusterableSymbol && center) {
      const [jx, jy] = hashJitter(`${projectId}:${node.id}:cluster`, clusterJitter);
      positions.set(node.id, { x: center.x + jx, y: center.y + jy });
      return;
    }

    const parentId = childToParent.get(id);
    const parentPosition = parentId ? positions.get(parentId) : undefined;
    if (parentPosition) {
      const [jx, jy] = hashJitter(`${projectId}:${node.id}:child`, childJitter);
      positions.set(node.id, {
        x: parentPosition.x + jx,
        y: parentPosition.y + jy,
      });
      return;
    }

    const [jx, jy] = hashJitter(
      `${projectId}:${node.id}:orphan`,
      structuralSpread * 0.5,
    );
    positions.set(node.id, { x: jx, y: jy });
  };

  const queue = structuralNodes.map((node) => node.id).sort(compareIds);
  const visited = new Set(queue);
  while (queue.length > 0) {
    const current = queue.shift()!;
    for (const childId of parentToChildren.get(current) ?? []) {
      if (visited.has(childId)) continue;
      visited.add(childId);
      placeNode(childId);
      queue.push(childId);
    }
  }

  for (const node of nodes) placeNode(node.id);
  return positions;
}

export function computeSequentialLayout(
  snapshot: LayoutInput,
): Map<string, LayoutPosition> {
  const { nodes } = normalizeLayoutInput(snapshot);
  const buckets: LayoutNode[][] = [[], [], [], [], []];

  for (const node of nodes) {
    if (node.kind === "folder") buckets[0].push(node);
    else if (node.kind === "file" || node.kind === "community") buckets[1].push(node);
    else {
      buckets[2 + symbolSubBucket(node)].push(node);
    }
  }

  const nonEmptyLayerIndices = buckets
    .map((bucket, index) => ({ bucket, index }))
    .filter(({ bucket }) => bucket.length > 0)
    .map(({ index }) => index);
  const middleLayer = (buckets.length - 1) / 2;
  const positions = new Map<string, LayoutPosition>();

  buckets.forEach((bucket, layerIndex) => {
    bucket.sort(compareNodesById);
    const layerCount = bucket.length;
    if (layerCount === 0) return;
    const yOffset = (layerIndex - middleLayer) * DEFAULT_LAYER_GAP;
    bucket.forEach((node, index) => {
      positions.set(
        node.id,
        spiralPosition(index, layerCount, DEFAULT_LAYER_SPREAD, yOffset),
      );
    });
  });

  // If the snapshot only contains one kind of node, center the occupied band so
  // community-only collapsed snapshots compose like a normal graph instead of
  // appearing arbitrarily above/below the viewport.
  if (nonEmptyLayerIndices.length === 1) {
    const onlyLayer = nonEmptyLayerIndices[0];
    const currentOffset = (onlyLayer - middleLayer) * DEFAULT_LAYER_GAP;
    for (const [id, position] of positions) {
      positions.set(id, { x: position.x, y: position.y - currentOffset });
    }
  }

  return positions;
}

export function computeRadialLayout(
  snapshot: LayoutInput,
  focalNodeId: string | null,
): Map<string, LayoutPosition> {
  const normalized = normalizeLayoutInput(snapshot);
  const { nodes } = normalized;
  const nodeIds = new Set(nodes.map((node) => node.id));
  if (nodes.length === 0) return new Map();

  const focalId =
    focalNodeId && nodeIds.has(focalNodeId)
      ? focalNodeId
      : highestPagerankNodeId(nodes);
  const shells = bfsShells(normalized, focalId);
  const maxReachableShell = Math.max(0, ...shells.values());
  const outerShell = maxReachableShell + 1;
  for (const node of nodes) {
    if (!shells.has(node.id)) shells.set(node.id, outerShell);
  }

  const nodesByShell = new Map<number, LayoutNode[]>();
  for (const node of nodes) {
    const shell = shells.get(node.id) ?? outerShell;
    const shellNodes = nodesByShell.get(shell) ?? [];
    shellNodes.push(node);
    nodesByShell.set(shell, shellNodes);
  }

  const positions = new Map<string, LayoutPosition>();
  const orderedShells = [...nodesByShell.keys()].sort((a, b) => a - b);
  for (const shell of orderedShells) {
    const shellNodes = nodesByShell.get(shell)!;
    shellNodes.sort(compareNodesById);
    if (shell === 0) {
      for (const node of shellNodes) positions.set(node.id, { x: 0, y: 0 });
      continue;
    }
    const radius = DEFAULT_RADIAL_BASE_RADIUS * shell;
    shellNodes.forEach((node, index) => {
      positions.set(node.id, radialShellPosition(index, radius));
    });
  }

  return positions;
}

function normalizeLayoutInput(input: LayoutInput): {
  projectId: string;
  nodes: LayoutNode[];
  edges: LayoutEdge[];
} {
  if (isSnapshotPayload(input)) {
    return {
      projectId: input.project_id,
      nodes: input.nodes.map((node) => ({
        id: node.id,
        kind: node.kind,
        symbol_kind: node.symbol_kind,
        pagerank: node.pagerank,
        community_id: node.community_id,
      })).sort(compareNodesById),
      edges: input.edges.map((edge) => ({
        from: edge.from,
        to: edge.to,
        kind: edge.kind,
      })),
    };
  }

  const nodes: LayoutNode[] = [];
  input.forEachNode((id: string, attributes: Record<string, unknown>) => {
    nodes.push({
      id,
      kind: normalizeKind(attributes.kind),
      symbol_kind:
        typeof attributes.symbolKind === "string"
          ? attributes.symbolKind
          : typeof attributes.symbol_kind === "string"
            ? attributes.symbol_kind
            : undefined,
      pagerank: finiteOrDefault(attributes.pagerank, 0),
      community_id:
        typeof attributes.communityId === "string"
          ? attributes.communityId
          : typeof attributes.community_id === "string"
            ? attributes.community_id
            : undefined,
    });
  });

  const edges: LayoutEdge[] = [];
  input.forEachEdge(
    (
      _edge: string,
      _attributes: Record<string, unknown>,
      source: string,
      target: string,
    ) => {
      edges.push({ from: source, to: target, kind: "" });
    },
  );

  return {
    projectId: String(input.getAttribute("project_id") ?? "graph"),
    nodes: nodes.sort(compareNodesById),
    edges,
  };
}

function isSnapshotPayload(input: LayoutInput): input is SnapshotPayload {
  return Array.isArray((input as SnapshotPayload).nodes);
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

function hierarchyParents(
  edges: readonly LayoutEdge[],
  nodeMap: ReadonlyMap<string, LayoutNode>,
): Map<string, string> {
  const childToParent = new Map<string, string>();
  for (const edge of edges) {
    if (!HIERARCHY_KINDS.has(edge.kind)) continue;
    if (!nodeMap.has(edge.from) || !nodeMap.has(edge.to)) continue;
    if (!childToParent.has(edge.to)) childToParent.set(edge.to, edge.from);
  }
  return childToParent;
}

function bfsShells(
  normalized: { nodes: LayoutNode[]; edges: LayoutEdge[] },
  focalNodeId: string,
): Map<string, number> {
  const adjacency = new Map<string, string[]>();
  for (const node of normalized.nodes) adjacency.set(node.id, []);
  for (const edge of normalized.edges) {
    if (!adjacency.has(edge.from) || !adjacency.has(edge.to)) continue;
    adjacency.get(edge.from)!.push(edge.to);
    adjacency.get(edge.to)!.push(edge.from);
  }
  for (const neighbors of adjacency.values()) neighbors.sort(compareIds);

  const shells = new Map<string, number>([[focalNodeId, 0]]);
  const queue = [focalNodeId];
  while (queue.length > 0) {
    const current = queue.shift()!;
    const nextShell = (shells.get(current) ?? 0) + 1;
    for (const neighbor of adjacency.get(current) ?? []) {
      if (shells.has(neighbor)) continue;
      shells.set(neighbor, nextShell);
      queue.push(neighbor);
    }
  }
  return shells;
}

function highestPagerankNodeId(nodes: readonly LayoutNode[]): string {
  return [...nodes].sort((a, b) => {
    const rankDelta = b.pagerank - a.pagerank;
    if (rankDelta !== 0) return rankDelta;
    return compareIds(a.id, b.id);
  })[0]!.id;
}

function symbolSubBucket(node: LayoutNode): 0 | 1 | 2 {
  const priority = SYMBOL_SUB_BUCKET_PRIORITY.get(node.symbol_kind ?? "other") ?? 2;
  return priority === 0 || priority === 1 ? priority : 2;
}

function spiralPosition(
  index: number,
  count: number,
  spread: number,
  yOffset: number,
): LayoutPosition {
  const angle = index * GOLDEN_ANGLE;
  const radius = spread * Math.sqrt((index + 1) / Math.max(count, 1));
  return {
    x: radius * Math.cos(angle),
    y: yOffset + radius * Math.sin(angle),
  };
}

function radialShellPosition(index: number, radius: number): LayoutPosition {
  const angle = index * GOLDEN_ANGLE;
  return {
    x: radius * Math.cos(angle),
    y: radius * Math.sin(angle),
  };
}

function hashJitter(seed: string, extent: number): [number, number] {
  const random = mulberry32(hashString(seed));
  return [(random() - 0.5) * extent, (random() - 0.5) * extent];
}

function hashString(value: string): number {
  let hash = 0x811c9dc5;
  for (let index = 0; index < value.length; index += 1) {
    hash ^= value.charCodeAt(index);
    hash = Math.imul(hash, 0x01000193);
  }
  return hash >>> 0;
}

function mulberry32(seed: number): () => number {
  return () => {
    let value = (seed += 0x6d2b79f5);
    value = Math.imul(value ^ (value >>> 15), value | 1);
    value ^= value + Math.imul(value ^ (value >>> 7), value | 61);
    return ((value ^ (value >>> 14)) >>> 0) / 4294967296;
  };
}

function compareNodesById(a: LayoutNode, b: LayoutNode): number {
  return compareIds(a.id, b.id);
}

function compareIds(a: string, b: string): number {
  return a.localeCompare(b, "en");
}

function finiteOrDefault(value: unknown, fallback: number): number {
  return typeof value === "number" && Number.isFinite(value) ? value : fallback;
}
