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
 */

import Graph from "graphology";
import { callMcpTool } from "@/api/mcpClient";
import type { MemoryGraphOutput } from "@/api/generated/mcp-tools.gen";
import {
  clusterMemoryCommunities,
  type MemoryCommunityMetadata,
} from "@/lib/memoryCommunityClustering";

/** Node attribute name carrying the stable community id (16-hex sha256). */
export const MEMORY_COMMUNITY_ID_ATTRIBUTE = "communityId";
/** Node attribute name carrying the top-K community label (space-joined terms). */
export const MEMORY_COMMUNITY_LABEL_ATTRIBUTE = "communityLabel";

/**
 * Fetch the memory wikilink graph for a project via the `memory_graph` MCP tool.
 *
 * Returns `null` when the project has no notes / no edges so callers can render
 * the empty state without building an empty graphology instance.
 */
export async function fetchMemoryGraph(
  projectSlug: string,
): Promise<MemoryGraphOutput | null> {
  const result = await callMcpTool("memory_graph", { project: projectSlug });
  if (result && typeof result === "object" && result.error) {
    throw new Error(result.error);
  }
  if (!result || !Array.isArray(result.nodes) || result.nodes.length === 0) {
    return null;
  }
  return result;
}

/**
 * Build a graphology graph from a `memory_graph` payload WITHOUT running
 * community clustering. Used by tests that want to assert the raw topology.
 *
 * Nodes are seeded on a small golden-angle spiral so FA2 starts from a
 * non-degenerate layout (the code-graph adapter does the same for its
 * structural nodes). Edges that reference unknown node ids are dropped.
 */
export function buildMemoryGraphFromPayload(payload: MemoryGraphOutput): Graph {
  const graph = new Graph({ type: "undirected" });

  const goldenAngle = Math.PI * (3 - Math.sqrt(5));
  payload.nodes.forEach((node, index) => {
    if (graph.hasNode(node.id)) return;
    const radius = Math.sqrt(index + 1) * 6;
    const theta = (index + 1) * goldenAngle;
    graph.addNode(node.id, {
      label: node.title,
      title: node.title,
      x: Math.cos(theta) * radius,
      y: Math.sin(theta) * radius,
      size: 6 + Math.min(Math.log2((node.connection_count || 0) + 1) * 1.5, 6),
      color: "#94a3b8",
      noteType: node.note_type,
      permalink: node.permalink,
      folder: node.folder,
      connectionCount: node.connection_count,
    });
  });

  for (const edge of payload.edges) {
    if (!graph.hasNode(edge.source_id) || !graph.hasNode(edge.target_id)) {
      continue;
    }
    if (edge.source_id === edge.target_id) continue;
    if (!graph.hasEdge(edge.source_id, edge.target_id)) {
      graph.addEdge(edge.source_id, edge.target_id, {
        color: "#2d2d3d",
        size: 0.8,
        type: "curved",
      });
    }
  }

  return graph;
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
