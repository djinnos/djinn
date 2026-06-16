import Graph from "graphology";
import { describe, expect, it } from "vitest";

import { clusterMemoryCommunities } from "@/lib/memoryCommunityClustering";

function buildFixtureGraph(): Graph {
  const graph = new Graph({ type: "undirected" });

  graph.addNode("alpha", { label: "Agent Runtime Design" });
  graph.addNode("beta", { title: "Agent Runtime Hooks" });
  graph.addNode("gamma", { label: "Worker Runtime Roadmap" });
  graph.addNode("delta", { label: "Memory Retrieval Design" });
  graph.addNode("epsilon", { title: "Memory Retrieval Ranking" });
  graph.addNode("zeta", { label: "Context Retrieval Notes" });

  graph.addEdge("alpha", "beta");
  graph.addEdge("beta", "gamma");
  graph.addEdge("alpha", "gamma");
  graph.addEdge("delta", "epsilon");
  graph.addEdge("epsilon", "zeta");
  graph.addEdge("delta", "zeta");
  graph.addEdge("gamma", "delta");

  return graph;
}

describe("clusterMemoryCommunities", () => {
  it("returns a stable community-id set for a fixed graph", () => {
    const graph = buildFixtureGraph();

    const first = clusterMemoryCommunities(graph);
    const second = clusterMemoryCommunities(graph);

    const firstCommunityIds = [...new Set([...first.values()].map((c) => c.communityId))].sort();
    const secondCommunityIds = [...new Set([...second.values()].map((c) => c.communityId))].sort();

    expect(firstCommunityIds).toEqual(secondCommunityIds);
    expect(firstCommunityIds.length).toBeGreaterThan(0);
    expect(firstCommunityIds.every((id) => /^[0-9a-f]{16}$/.test(id))).toBe(true);
    expect(first.size).toBe(second.size);
  });

  it("derives deterministic labels from common normalized member terms", () => {
    const graph = new Graph({ type: "undirected" });
    graph.addNode("a", { label: "Memory Retrieval Design" });
    graph.addNode("b", { title: "Memory Retrieval Ranking" });
    graph.addNode("c", { label: "Retrieval Context Notes" });
    graph.addEdge("a", "b");
    graph.addEdge("b", "c");
    graph.addEdge("a", "c");

    const clustered = clusterMemoryCommunities(graph);

    expect(clustered.get("a")?.label).toBe("retrieval context ranking");
    expect(clustered.get("b")?.label).toBe("retrieval context ranking");
    expect(clustered.get("c")?.label).toBe("retrieval context ranking");
  });

  it("returns no clustered metadata for a single-node graph", () => {
    const graph = new Graph({ type: "undirected" });
    graph.addNode("lonely", { label: "Standalone Note" });

    expect(clusterMemoryCommunities(graph)).toEqual(new Map());
  });
});
