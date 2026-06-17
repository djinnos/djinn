import Graph from "graphology";
import { describe, expect, it } from "vitest";

import {
  applyPerCommunityAttraction,
  assignCommunityCenters,
  type PerCommunityAttractionMetadata,
} from "@/lib/perCommunityAttraction";

function distanceToCenter(
  graph: Graph,
  nodeId: string,
  center: { x: number; y: number },
): number {
  const x = graph.getNodeAttribute(nodeId, "x") as number;
  const y = graph.getNodeAttribute(nodeId, "y") as number;
  return Math.hypot(center.x - x, center.y - y);
}

describe("applyPerCommunityAttraction", () => {
  it("moves clustered nodes closer to their deterministic community center", () => {
    const graph = new Graph();
    graph.addNode("b1", { x: -100, y: 0 });
    graph.addNode("b2", { x: -120, y: 20 });
    graph.addNode("a1", { x: 100, y: 0 });
    graph.addNode("a2", { x: 120, y: -20 });

    const metadata: PerCommunityAttractionMetadata = new Map([
      ["b1", { communityId: "beta" }],
      ["b2", { communityId: "beta" }],
      ["a1", { communityId: "alpha" }],
      ["a2", { communityId: "alpha" }],
    ]);

    const result = applyPerCommunityAttraction(graph, metadata, {
      clusterRadius: 10,
      strength: 0.5,
      iterations: 1,
    });

    expect(result.orderedCommunityIds).toEqual(["alpha", "beta"]);
    expect(result.eligibleNodeCount).toBe(4);
    expect(result.movedNodeCount).toBe(4);
    expect(result.centers.get("alpha")).toEqual({ x: 10, y: 0 });
    expect(result.centers.get("beta")?.x).toBeCloseTo(-10);
    expect(result.centers.get("beta")?.y).toBeCloseTo(0);

    const alphaCenter = result.centers.get("alpha");
    expect(alphaCenter).toBeDefined();
    expect(distanceToCenter(graph, "a1", alphaCenter!)).toBeLessThan(90);
    expect(distanceToCenter(graph, "a2", alphaCenter!)).toBeLessThan(
      Math.hypot(10 - 120, 0 - -20),
    );
  });

  it("does not move nodes without attraction-eligible community metadata", () => {
    const graph = new Graph();
    graph.addNode("clustered-1", { x: 100, y: 100 });
    graph.addNode("clustered-2", { x: 120, y: 120 });
    graph.addNode("unclustered", { x: 7, y: -3 });
    graph.addNode("empty-community", { x: 9, y: 11 });

    const result = applyPerCommunityAttraction(
      graph,
      {
        "clustered-1": { communityId: "cluster" },
        "clustered-2": { communityId: "cluster" },
        "empty-community": { communityId: "   " },
      },
      { clusterRadius: 0, strength: 1, iterations: 1 },
    );

    expect(result.movedNodeCount).toBe(2);
    expect(graph.getNodeAttribute("unclustered", "x")).toBe(7);
    expect(graph.getNodeAttribute("unclustered", "y")).toBe(-3);
    expect(graph.getNodeAttribute("empty-community", "x")).toBe(9);
    expect(graph.getNodeAttribute("empty-community", "y")).toBe(11);
  });

  it("assigns community centers deterministically by sorted community id", () => {
    const first = assignCommunityCenters(["alpha", "beta", "gamma"], 30);
    const second = assignCommunityCenters(["alpha", "beta", "gamma"], 30);

    expect([...first.entries()]).toEqual([...second.entries()]);
    expect(first.get("alpha")).toEqual({ x: 30, y: 0 });
    expect(first.get("beta")?.x).toBeCloseTo(-15);
    expect(first.get("beta")?.y).toBeCloseTo(25.980762);
    expect(first.get("gamma")?.x).toBeCloseTo(-15);
    expect(first.get("gamma")?.y).toBeCloseTo(-25.980762);
  });

  it("treats empty and single-node graphs as no-ops", () => {
    const empty = new Graph();
    const emptyResult = applyPerCommunityAttraction(empty, new Map(), {
      strength: 1,
      iterations: 10,
    });
    expect(emptyResult.orderedCommunityIds).toEqual([]);
    expect(emptyResult.movedNodeCount).toBe(0);

    const single = new Graph();
    single.addNode("lonely", { x: 3, y: 4 });
    const singleResult = applyPerCommunityAttraction(
      single,
      new Map([["lonely", { communityId: "solo" }]]),
      { strength: 1, iterations: 10 },
    );

    expect(singleResult.orderedCommunityIds).toEqual([]);
    expect(singleResult.movedNodeCount).toBe(0);
    expect(single.getNodeAttribute("lonely", "x")).toBe(3);
    expect(single.getNodeAttribute("lonely", "y")).toBe(4);
  });
});
