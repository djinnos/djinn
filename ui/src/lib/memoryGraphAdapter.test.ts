/**
 * memoryGraphAdapter — integration tests for the memory graph → community
 * clustering → per-community attraction wiring.
 *
 * These tests prove the integration that `MemoryGraphCanvas` relies on:
 *   1. `buildClusteredMemoryGraph` builds the graphology graph AND runs
 *      `clusterMemoryCommunities`, writing `communityId`/`communityLabel`
 *      onto clustered nodes while leaving singletons/unclustered notes
 *      without those attributes.
 *   2. The single-node / no-community path is a no-op for the per-community
 *      attraction pass — `applyPerCommunityAttraction` returns
 *      `movedNodeCount: 0` and leaves node positions untouched.
 *   3. The community metadata map returned by `buildClusteredMemoryGraph`
 *      can be fed straight into `applyPerCommunityAttraction` and produces
 *      deterministic centers, mirroring how `MemoryGraphCanvas.postLayout`
 *      invokes the pass after FA2 settles.
 *
 * The full canvas component mount is covered by the targeted typecheck —
 * Sigma + WebGL can't initialize under jsdom, so we exercise the pure
 * adapter/attraction composition here instead.
 */

import { describe, expect, it } from "vitest";

import type { MemoryGraphOutput } from "@/api/generated/mcp-tools.gen";
import {
  buildClusteredMemoryGraph,
  buildMemoryGraphFromPayload,
  applyMemoryCommunities,
  MEMORY_COMMUNITY_ID_ATTRIBUTE,
  MEMORY_COMMUNITY_LABEL_ATTRIBUTE,
} from "@/lib/memoryGraphAdapter";
import { applyPerCommunityAttraction } from "@/lib/perCommunityAttraction";

function makePayload(overrides: Partial<MemoryGraphOutput> = {}): MemoryGraphOutput {
  return {
    nodes: [],
    edges: [],
    ...overrides,
  };
}

describe("buildClusteredMemoryGraph — community metadata wiring", () => {
  it("writes communityId and communityLabel onto clustered nodes", () => {
    const payload = makePayload({
      nodes: [
        { id: "a", title: "Agent Runtime Design", permalink: "design/a", folder: "design", note_type: "design", connection_count: 2 },
        { id: "b", title: "Agent Runtime Hooks", permalink: "design/b", folder: "design", note_type: "design", connection_count: 2 },
        { id: "c", title: "Worker Runtime Roadmap", permalink: "design/c", folder: "design", note_type: "design", connection_count: 2 },
        { id: "d", title: "Memory Retrieval Design", permalink: "design/d", folder: "design", note_type: "design", connection_count: 2 },
        { id: "e", title: "Memory Retrieval Ranking", permalink: "design/e", folder: "design", note_type: "design", connection_count: 2 },
        { id: "f", title: "Context Retrieval Notes", permalink: "design/f", folder: "design", note_type: "design", connection_count: 2 },
      ],
      edges: [
        { source_id: "a", target_id: "b", raw_text: "[[Agent Runtime Hooks]]" },
        { source_id: "b", target_id: "c", raw_text: "[[Worker Runtime Roadmap]]" },
        { source_id: "a", target_id: "c", raw_text: "[[Worker Runtime Roadmap]]" },
        { source_id: "d", target_id: "e", raw_text: "[[Memory Retrieval Ranking]]" },
        { source_id: "e", target_id: "f", raw_text: "[[Context Retrieval Notes]]" },
        { source_id: "d", target_id: "f", raw_text: "[[Context Retrieval Notes]]" },
        { source_id: "c", target_id: "d", raw_text: "[[Memory Retrieval Design]]" },
      ],
    });

    const { graph, communities } = buildClusteredMemoryGraph(payload);

    expect(communities.size).toBeGreaterThan(0);

    // Every clustered node carries both community attributes.
    for (const [nodeId, metadata] of communities) {
      expect(graph.getNodeAttribute(nodeId, MEMORY_COMMUNITY_ID_ATTRIBUTE)).toBe(
        metadata.communityId,
      );
      expect(graph.getNodeAttribute(nodeId, MEMORY_COMMUNITY_LABEL_ATTRIBUTE)).toBe(
        metadata.label,
      );
      expect(metadata.communityId).toMatch(/^[0-9a-f]{16}$/);
    }

    // Community ids are stable across a second build of the same payload.
    const second = buildClusteredMemoryGraph(payload);
    const firstIds = [...new Set([...communities.values()].map((c) => c.communityId))].sort();
    const secondIds = [...new Set([...second.communities.values()].map((c) => c.communityId))].sort();
    expect(secondIds).toEqual(firstIds);
  });

  it("leaves singleton/unclustered nodes without community attributes", () => {
    const payload = makePayload({
      nodes: [
        { id: "hub", title: "Hub Note", permalink: "p/hub", folder: "p", note_type: "design", connection_count: 1 },
        { id: "hub-friend", title: "Hub Friend", permalink: "p/hub-friend", folder: "p", note_type: "design", connection_count: 1 },
        { id: "lonely", title: "Lonely Note", permalink: "p/lonely", folder: "p", note_type: "design", connection_count: 0 },
      ],
      edges: [
        { source_id: "hub", target_id: "hub-friend", raw_text: "[[Hub Friend]]" },
      ],
    });

    const { graph, communities } = buildClusteredMemoryGraph(payload);

    // The connected pair clusters; the isolated singleton does not.
    expect(communities.has("hub")).toBe(true);
    expect(communities.has("hub-friend")).toBe(true);
    expect(communities.has("lonely")).toBe(false);

    // Clustered nodes carry the attributes.
    expect(graph.getNodeAttribute("hub", MEMORY_COMMUNITY_ID_ATTRIBUTE)).toBeDefined();
    expect(graph.getNodeAttribute("hub-friend", MEMORY_COMMUNITY_ID_ATTRIBUTE)).toBeDefined();

    // The singleton has no community attributes — reducers can detect
    // absence and keep default styling, and the attraction pass skips it.
    expect(graph.getNodeAttribute("lonely", MEMORY_COMMUNITY_ID_ATTRIBUTE)).toBeUndefined();
    expect(graph.getNodeAttribute("lonely", MEMORY_COMMUNITY_LABEL_ATTRIBUTE)).toBeUndefined();
  });
});

describe("per-community attraction no-op guarantees", () => {
  it("is a no-op for a single-node graph (no communities)", () => {
    const payload = makePayload({
      nodes: [
        { id: "lonely", title: "Standalone Note", permalink: "p/lonely", folder: "p", note_type: "design", connection_count: 0 },
      ],
      edges: [],
    });

    const { graph, communities } = buildClusteredMemoryGraph(payload);

    // No clustered communities at all.
    expect(communities.size).toBe(0);

    const beforeX = graph.getNodeAttribute("lonely", "x");
    const beforeY = graph.getNodeAttribute("lonely", "y");

    const result = applyPerCommunityAttraction(graph, communities, {
      clusterRadius: 400,
      strength: 0.1,
    });

    expect(result.orderedCommunityIds).toEqual([]);
    expect(result.eligibleNodeCount).toBe(0);
    expect(result.movedNodeCount).toBe(0);
    expect(graph.getNodeAttribute("lonely", "x")).toBe(beforeX);
    expect(graph.getNodeAttribute("lonely", "y")).toBe(beforeY);
  });

  it("is a no-op when no node carries community metadata", () => {
    // A graph with nodes but no clustering applied — mirrors the unclustered
    // state. `applyMemoryCommunities` on an edgeless graph returns an empty map.
    const graph = buildMemoryGraphFromPayload(
      makePayload({
        nodes: [
          { id: "a", title: "Isolated A", permalink: "p/a", folder: "p", note_type: "design", connection_count: 0 },
          { id: "b", title: "Isolated B", permalink: "p/b", folder: "p", note_type: "design", connection_count: 0 },
        ],
        edges: [],
      }),
    );

    const communities = applyMemoryCommunities(graph);
    expect(communities.size).toBe(0);

    const positionsBefore = new Map<string, { x: number; y: number }>();
    graph.forEachNode((id, attrs) => {
      positionsBefore.set(id, { x: attrs.x, y: attrs.y });
    });

    const result = applyPerCommunityAttraction(graph, communities);
    expect(result.movedNodeCount).toBe(0);

    graph.forEachNode((id, attrs) => {
      const before = positionsBefore.get(id)!;
      expect(attrs.x).toBe(before.x);
      expect(attrs.y).toBe(before.y);
    });
  });
});

describe("postLayout composition (FA2 → attraction ordering)", () => {
  it("moving clustered nodes toward deterministic centers leaves unclustered nodes untouched", () => {
    // Simulate the post-FA2 state: clustered nodes have settled positions,
    // an unclustered node sits somewhere else. The attraction pass must pull
    // only the clustered nodes toward their community center.
    const payload = makePayload({
      nodes: [
        { id: "a", title: "Alpha Cluster", permalink: "p/a", folder: "p", note_type: "design", connection_count: 2 },
        { id: "b", title: "Alpha Cluster", permalink: "p/b", folder: "p", note_type: "design", connection_count: 2 },
        { id: "c", title: "Beta Cluster", permalink: "p/c", folder: "p", note_type: "design", connection_count: 2 },
        { id: "d", title: "Beta Cluster", permalink: "p/d", folder: "p", note_type: "design", connection_count: 2 },
        { id: "lonely", title: "Lonely", permalink: "p/lonely", folder: "p", note_type: "design", connection_count: 0 },
      ],
      edges: [
        { source_id: "a", target_id: "b", raw_text: "[[Alpha Cluster]]" },
        { source_id: "c", target_id: "d", raw_text: "[[Beta Cluster]]" },
      ],
    });

    const { graph, communities } = buildClusteredMemoryGraph(payload);

    // Scatter the clustered nodes far from origin to simulate post-FA2 spread.
    graph.setNodeAttribute("a", "x", 500);
    graph.setNodeAttribute("a", "y", 500);
    graph.setNodeAttribute("b", "x", -500);
    graph.setNodeAttribute("b", "y", -500);
    graph.setNodeAttribute("c", "x", 300);
    graph.setNodeAttribute("c", "y", -300);
    graph.setNodeAttribute("d", "x", -300);
    graph.setNodeAttribute("d", "y", 300);
    const lonelyX = 42;
    const lonelyY = -17;
    graph.setNodeAttribute("lonely", "x", lonelyX);
    graph.setNodeAttribute("lonely", "y", lonelyY);

    const result = applyPerCommunityAttraction(graph, communities, {
      clusterRadius: 50,
      strength: 0.5,
      iterations: 10,
    });

    // Two distinct communities were detected.
    expect(result.orderedCommunityIds.length).toBe(2);
    expect(result.eligibleNodeCount).toBe(4);
    expect(result.movedNodeCount).toBe(4);

    // The unclustered node is untouched — it never had community metadata.
    expect(graph.getNodeAttribute("lonely", "x")).toBe(lonelyX);
    expect(graph.getNodeAttribute("lonely", "y")).toBe(lonelyY);

    // Clustered nodes moved toward their assigned centers (distance shrunk).
    for (const nodeId of ["a", "b", "c", "d"]) {
      const communityId = graph.getNodeAttribute(nodeId, MEMORY_COMMUNITY_ID_ATTRIBUTE) as string;
      const center = result.centers.get(communityId);
      expect(center).toBeDefined();
      const x = graph.getNodeAttribute(nodeId, "x") as number;
      const y = graph.getNodeAttribute(nodeId, "y") as number;
      // After multiple iterations at strength 0.5 the nodes should be
      // measurably closer to their center than their starting 300-500 spread.
      const dist = Math.hypot(center!.x - x, center!.y - y);
      expect(dist).toBeLessThan(100);
    }
  });
});

describe("empty payload handling", () => {
  it("buildMemoryGraphFromPayload handles an empty node list", () => {
    const graph = buildMemoryGraphFromPayload(makePayload());
    expect(graph.order).toBe(0);
    expect(graph.size).toBe(0);
  });

  it("drops edges referencing unknown nodes", () => {
    const graph = buildMemoryGraphFromPayload(
      makePayload({
        nodes: [
          { id: "a", title: "A", permalink: "p/a", folder: "p", note_type: "design", connection_count: 0 },
        ],
        edges: [
          { source_id: "a", target_id: "ghost", raw_text: "[[ghost]]" },
        ],
      }),
    );
    expect(graph.order).toBe(1);
    expect(graph.size).toBe(0);
  });
});
