import { describe, expect, it } from "vitest";
import {
  buildGraphFromSnapshot,
  massForNode,
  parseSnapshotResponse,
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
    expect(fileAttrs.mass).toBe(3); // FILE mass
    expect(fileAttrs.pagerank).toBeCloseTo(0.4);

    const classAttrs = graph.getNodeAttributes("symbol:scip-rust . . . User#");
    expect(classAttrs.kind).toBe("symbol");
    expect(classAttrs.symbolKind).toBe("class");
    expect(classAttrs.mass).toBe(2); // SYMBOL mass
  });

  it("sizes nodes proportional to pagerank within [3, 18]", () => {
    const graph = buildGraphFromSnapshot(fixtureSnapshot);
    for (const id of graph.nodes()) {
      const size = graph.getNodeAttribute(id, "size");
      expect(size).toBeGreaterThanOrEqual(3);
      expect(size).toBeLessThanOrEqual(18 + 0.001);
    }
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
});

describe("massForNode", () => {
  it("returns the SCIP-kind override for symbols", () => {
    expect(
      massForNode({
        id: "x",
        kind: "symbol",
        label: "x",
        pagerank: 0,
        symbol_kind: "class",
      }),
    ).toBe(2);
  });

  it("falls back to the top-level kind when symbol_kind is absent", () => {
    expect(
      massForNode({ id: "x", kind: "file", label: "x", pagerank: 0 }),
    ).toBe(3);
  });
});
