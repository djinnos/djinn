import { describe, expect, it } from "vitest";
import {
  COMMUNITY_COLORS,
  buildGraphFromSnapshot,
  colorForCommunity,
  colorForNode,
  edgeStyleFor,
  massForNode,
  parseSnapshotResponse,
  type SnapshotNode,
  type SnapshotPayload,
} from "@/lib/codeGraphAdapter";

const fixtureSnapshot: SnapshotPayload = {
  project_id: "proj-test",
  git_head: "deadbeef",
  generated_at: "2026-04-28T00:00:00Z",
  truncated: false,
  total_nodes: 4,
  total_edges: 3,
  node_cap: 2_000,
  nodes: [
    {
      id: "file:src/main.rs",
      kind: "file",
      label: "main.rs",
      pagerank: 0.4,
    },
    {
      id: "symbol:scip-rust . . . main()",
      kind: "symbol",
      label: "main",
      symbol_kind: "function",
      file_path: "src/main.rs",
      pagerank: 0.3,
    },
    {
      id: "symbol:scip-rust . . . User#",
      kind: "symbol",
      label: "User",
      symbol_kind: "class",
      file_path: "src/user.rs",
      pagerank: 0.2,
    },
    {
      id: "file:src/user.rs",
      kind: "file",
      label: "user.rs",
      pagerank: 0.1,
    },
  ],
  edges: [
    {
      from: "file:src/main.rs",
      to: "symbol:scip-rust . . . main()",
      kind: "ContainsDefinition",
      confidence: 0.95,
    },
    {
      from: "symbol:scip-rust . . . main()",
      to: "symbol:scip-rust . . . User#",
      kind: "SymbolReference",
      confidence: 0.85,
      reason: "calls",
    },
    {
      from: "file:src/user.rs",
      to: "symbol:scip-rust . . . User#",
      kind: "ContainsDefinition",
      confidence: 0.95,
    },
  ],
};

describe("parseSnapshotResponse", () => {
  it("narrows the untagged response into the typed payload", () => {
    const wire = { snapshot: fixtureSnapshot, next_step: null };
    const parsed = parseSnapshotResponse(wire);
    expect(parsed).not.toBeNull();
    expect(parsed?.project_id).toBe("proj-test");
    expect(parsed?.nodes).toHaveLength(4);
    expect(parsed?.edges).toHaveLength(3);
  });

  it("returns null for non-snapshot variants", () => {
    expect(parseSnapshotResponse({ nodes: [] })).toBeNull();
    expect(parseSnapshotResponse({ symbol_context: {} })).toBeNull();
    expect(parseSnapshotResponse(null)).toBeNull();
  });

  it("drops nodes / edges with empty ids", () => {
    const wire = {
      snapshot: {
        ...fixtureSnapshot,
        nodes: [...fixtureSnapshot.nodes, { id: "", kind: "file", label: "", pagerank: 0 }],
        edges: [
          ...fixtureSnapshot.edges,
          { from: "", to: "x", kind: "X", confidence: 0 },
        ],
      },
    };
    const parsed = parseSnapshotResponse(wire);
    expect(parsed?.nodes).toHaveLength(4);
    expect(parsed?.edges).toHaveLength(3);
  });

  it("preserves community_id on nodes when present", () => {
    const wire = {
      snapshot: {
        ...fixtureSnapshot,
        nodes: [
          {
            ...fixtureSnapshot.nodes[1],
            community_id: "cluster-7",
          },
        ],
      },
    };
    const parsed = parseSnapshotResponse(wire);
    expect(parsed?.nodes[0]?.community_id).toBe("cluster-7");
  });
});

describe("buildGraphFromSnapshot", () => {
  it("emits one graphology node per snapshot node and one edge per snapshot edge", () => {
    const graph = buildGraphFromSnapshot(fixtureSnapshot);
    expect(graph.order).toBe(fixtureSnapshot.nodes.length);
    expect(graph.size).toBe(fixtureSnapshot.edges.length);
  });

  it("attaches per-type mass, kind, and pagerank to each node", () => {
    const graph = buildGraphFromSnapshot(fixtureSnapshot);
    const fileAttrs = graph.getNodeAttributes("file:src/main.rs");
    expect(fileAttrs.kind).toBe("file");
    expect(fileAttrs.mass).toBe(3); // FILE mass at small node count
    expect(fileAttrs.pagerank).toBeCloseTo(0.4);

    const classAttrs = graph.getNodeAttributes("symbol:scip-rust . . . User#");
    expect(classAttrs.kind).toBe("symbol");
    expect(classAttrs.symbolKind).toBe("class");
    expect(classAttrs.mass).toBe(5); // class symbols anchor methods
  });

  it("seeds structural nodes on a deterministic-radius spiral, not at the origin", () => {
    const graph = buildGraphFromSnapshot(fixtureSnapshot);
    let allOrigin = true;
    for (const id of graph.nodes()) {
      const x = graph.getNodeAttribute(id, "x") as number;
      const y = graph.getNodeAttribute(id, "y") as number;
      if (Math.abs(x) > 0.5 || Math.abs(y) > 0.5) {
        allOrigin = false;
        break;
      }
    }
    expect(allOrigin).toBe(false);
  });

  it("pre-positions cluster-tagged symbols near a community center", () => {
    const withCommunity: SnapshotPayload = {
      ...fixtureSnapshot,
      nodes: fixtureSnapshot.nodes.map((n) =>
        n.kind === "symbol" ? { ...n, community_id: "alpha" } : n,
      ),
    };
    const graph = buildGraphFromSnapshot(withCommunity);
    const a = graph.getNodeAttributes("symbol:scip-rust . . . main()") as Record<
      string,
      unknown
    >;
    const b = graph.getNodeAttributes("symbol:scip-rust . . . User#") as Record<
      string,
      unknown
    >;
    expect(a.communityId).toBe("alpha");
    expect(b.communityId).toBe("alpha");
    // Same community → both should sit within `clusterJitter` of the
    // single cluster center; concretely they're closer to each other
    // than to the origin in the worst case.
    const dx = (a.x as number) - (b.x as number);
    const dy = (a.y as number) - (b.y as number);
    const dist = Math.sqrt(dx * dx + dy * dy);
    // clusterJitter for 4 nodes ≈ sqrt(4)*1.5 = 3, so max separation
    // is 3 (jitter on each axis × √2). Generous bound: 6.
    expect(dist).toBeLessThan(6);
  });

  it("drops self-loops by default", () => {
    const withLoop: SnapshotPayload = {
      ...fixtureSnapshot,
      edges: [
        ...fixtureSnapshot.edges,
        {
          from: "file:src/main.rs",
          to: "file:src/main.rs",
          kind: "FileReference",
          confidence: 0.9,
        },
      ],
    };
    const graph = buildGraphFromSnapshot(withLoop);
    expect(graph.size).toBe(fixtureSnapshot.edges.length);
  });

  it("drops edges whose endpoints aren't in the node set", () => {
    const withDangling: SnapshotPayload = {
      ...fixtureSnapshot,
      edges: [
        ...fixtureSnapshot.edges,
        {
          from: "file:src/main.rs",
          to: "file:src/missing.rs",
          kind: "FileReference",
          confidence: 0.9,
        },
      ],
    };
    const graph = buildGraphFromSnapshot(withDangling);
    expect(graph.size).toBe(fixtureSnapshot.edges.length);
  });

  it("paints edges with the per-kind color", () => {
    const graph = buildGraphFromSnapshot(fixtureSnapshot);
    const containsEdges = graph
      .edges()
      .filter(
        (e) => graph.getEdgeAttribute(e, "kind") === "ContainsDefinition",
      );
    for (const e of containsEdges) {
      expect(graph.getEdgeAttribute(e, "color")).toBe("#2d5a3d");
    }
  });

  it("can drop MemberOf edges via option", () => {
    const withMember: SnapshotPayload = {
      ...fixtureSnapshot,
      edges: [
        ...fixtureSnapshot.edges,
        {
          from: "symbol:scip-rust . . . User#",
          to: "file:src/user.rs",
          kind: "MemberOf",
          confidence: 1.0,
        },
      ],
    };
    const noDrop = buildGraphFromSnapshot(withMember);
    expect(noDrop.size).toBe(withMember.edges.length);
    const dropped = buildGraphFromSnapshot(withMember, { dropMemberOf: true });
    expect(dropped.size).toBe(fixtureSnapshot.edges.length);
  });
});

describe("massForNode", () => {
  it("class-like symbols get mass 5", () => {
    expect(
      massForNode({
        id: "x",
        kind: "symbol",
        label: "x",
        pagerank: 0,
        symbol_kind: "class",
      }),
    ).toBe(5);
  });

  it("function-like symbols get mass 2", () => {
    expect(
      massForNode({
        id: "x",
        kind: "symbol",
        label: "x",
        pagerank: 0,
        symbol_kind: "function",
      }),
    ).toBe(2);
  });

  it("file gets mass 3", () => {
    expect(
      massForNode({ id: "x", kind: "file", label: "x", pagerank: 0 }),
    ).toBe(3);
  });

  it("folder gets mass 15", () => {
    expect(
      massForNode({ id: "x", kind: "folder", label: "src", pagerank: 0 }),
    ).toBe(15);
  });

  describe("scales with node count", () => {
    const node: SnapshotNode = {
      id: "x",
      kind: "file",
      label: "x",
      pagerank: 0,
    };
    it("uses 1× under 1000 nodes", () => {
      expect(massForNode(node, 500)).toBe(3);
    });
    it("uses 1.5× between 1k and 5k", () => {
      expect(massForNode(node, 2000)).toBe(4.5);
    });
    it("uses 2× above 5k", () => {
      expect(massForNode(node, 8000)).toBe(6);
    });
  });
});

describe("colorForNode", () => {
  it("colors symbols by community_id when present", () => {
    const sym: SnapshotNode = {
      id: "s",
      kind: "symbol",
      label: "x",
      pagerank: 0,
      symbol_kind: "function",
      community_id: "cluster-7",
      file_path: "any/path/file.ts",
    };
    expect(colorForNode(sym)).toBe(colorForCommunity("cluster-7"));
  });

  it("falls back to top-level folder hash when community_id is absent", () => {
    const sym: SnapshotNode = {
      id: "s",
      kind: "symbol",
      label: "x",
      pagerank: 0,
      symbol_kind: "function",
      file_path: "server/crates/djinn-graph/src/lib.rs",
    };
    expect(colorForNode(sym)).toBe(colorForCommunity("server"));
  });

  it("returns a fixed color for files / folders", () => {
    const file: SnapshotNode = {
      id: "f",
      kind: "file",
      label: "main.rs",
      pagerank: 0,
    };
    expect(colorForNode(file)).toBe("#3b82f6");
    const folder: SnapshotNode = {
      id: "d",
      kind: "folder",
      label: "src",
      pagerank: 0,
    };
    expect(colorForNode(folder)).toBe("#6366f1");
  });

  it("colorForCommunity is deterministic across calls", () => {
    expect(colorForCommunity("alpha")).toBe(colorForCommunity("alpha"));
  });

  it("colorForCommunity distributes distinct community ids across the palette", () => {
    const seen = new Set<string>();
    for (const cid of ["c0", "c1", "c2", "c3", "c4", "c5", "c6", "c7"]) {
      seen.add(colorForCommunity(cid));
    }
    // 8 distinct ids over a 12-hue palette should yield several
    // different colors — this guards against the hash bucketing
    // everything to a single hue.
    expect(seen.size).toBeGreaterThanOrEqual(4);
  });

  it("colorForCommunity always returns a color from the 12-hue palette", () => {
    for (const cid of ["a", "b", "c", "test", "cluster-99", "longer-id-here"]) {
      expect(COMMUNITY_COLORS).toContain(colorForCommunity(cid));
    }
  });
});

describe("edgeStyleFor", () => {
  it("returns the per-kind color and size multiplier", () => {
    expect(edgeStyleFor("ContainsDefinition").color).toBe("#2d5a3d");
    expect(edgeStyleFor("ContainsDefinition").sizeMultiplier).toBeCloseTo(0.4);
    expect(edgeStyleFor("SymbolReference").color).toBe("#7c3aed");
    expect(edgeStyleFor("Writes").color).toBe("#dc2626");
    expect(edgeStyleFor("Extends").color).toBe("#c2410c");
    expect(edgeStyleFor("Implements").color).toBe("#be185d");
    expect(edgeStyleFor("MemberOf").color).toBe("#1e293b");
  });

  it("returns a neutral fallback for unknown kinds", () => {
    const fallback = edgeStyleFor("MysteryKind");
    expect(fallback.color).toBe("#4a4a5a");
  });
});
