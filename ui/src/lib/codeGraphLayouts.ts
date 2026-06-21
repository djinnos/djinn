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

const HIERARCHY_KINDS = new Set([
  "ContainsDefinition",
  "DeclaredInFile",
  "MemberOf",
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

/**
 * Deterministic seed positions for the force path. ForceAtlas2 can still run on
 * top of these positions, but this helper itself is pure and never calls
 * a nondeterministic random source.
 */
export function computeForceLayout(
  snapshot: LayoutInput,
  options: ForceLayoutOptions = {},
): Map<string, LayoutPosition> {
  const { nodes: allNodes, edges, projectId } = normalizeLayoutInput(snapshot);
  // Communities are background hulls, not positioned layout nodes.
  // Filter them out so they never receive seed positions — the adapter
  // also strips them before calling this function, but the layout must
  // be defensive in case it's called directly with a raw snapshot.
  const nodes = allNodes.filter((node) => node.kind !== "community");
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
    (node) => node.kind === "folder" || node.kind === "file",
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
    const parent =
      edge.kind === "ContainsDefinition" || edge.kind === "FileReference"
        ? edge.from
        : edge.to;
    const child =
      edge.kind === "ContainsDefinition" || edge.kind === "FileReference"
        ? edge.to
        : edge.from;
    if (!childToParent.has(child)) childToParent.set(child, parent);
  }
  return childToParent;
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
