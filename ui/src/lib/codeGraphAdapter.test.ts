import { describe, expect, it } from "vitest";
import {
  COMMUNITY_COLORS,
  PRECOMPUTED_LAYOUT_ATTRIBUTE,
  WORKSPACE_COLORS,
  buildGraphFromSnapshot,
  colorForCommunity,
  colorForNode,
  colorForWorkspace,
  edgeStyleFor,
  filterSnapshotForWorkspace,
  hasPrecomputedCoordinates,
  massForNode,
  parseSnapshotResponse,
  prettifyLabel,
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

  it("coerces null symbol_kind on symbol nodes to 'other' so the filter catches them", () => {
    const wire = {
      snapshot: {
        ...fixtureSnapshot,
        nodes: [
          {
            id: "symbol:scip-go gomod github.com/golang/go/src . context/Context#",
            kind: "symbol",
            label: "scip-go gomod github.com/golang/go/src . context/Context#",
            symbol_kind: null,
            pagerank: 0.1,
          },
        ],
      },
    };
    const parsed = parseSnapshotResponse(wire);
    expect(parsed?.nodes[0]?.symbol_kind).toBe("other");
    expect(parsed?.nodes[0]?.label).toBe("Context");
  });

  it("preserves cognitive on nodes when present (iter 30)", () => {
    const wire = {
      snapshot: {
        ...fixtureSnapshot,
        nodes: [
          {
            ...fixtureSnapshot.nodes[1],
            cognitive: 17,
          },
        ],
      },
    };
    const parsed = parseSnapshotResponse(wire);
    expect(parsed?.nodes[0]?.cognitive).toBe(17);
  });

  it("treats non-numeric / missing cognitive as undefined (iter 30)", () => {
    const wire = {
      snapshot: {
        ...fixtureSnapshot,
        nodes: [
          { ...fixtureSnapshot.nodes[1], cognitive: null },
          { ...fixtureSnapshot.nodes[2], cognitive: "huge" as unknown as number },
        ],
      },
    };
    const parsed = parseSnapshotResponse(wire);
    expect(parsed?.nodes[0]?.cognitive).toBeUndefined();
    expect(parsed?.nodes[1]?.cognitive).toBeUndefined();
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

  it("preserves non-empty workspace tags on nodes when present", () => {
    const wire = {
      snapshot: {
        ...fixtureSnapshot,
        nodes: [
          {
            ...fixtureSnapshot.nodes[1],
            workspace: "api",
          },
        ],
      },
    };
    const parsed = parseSnapshotResponse(wire);
    expect(parsed?.nodes[0]?.workspace).toBe("api");
  });

  it("leaves missing workspace tags undefined", () => {
    const parsed = parseSnapshotResponse({ snapshot: fixtureSnapshot });
    expect(parsed?.nodes[0]?.workspace).toBeUndefined();
  });

  it("drops invalid or blank workspace tags", () => {
    const wire = {
      snapshot: {
        ...fixtureSnapshot,
        nodes: [
          { ...fixtureSnapshot.nodes[0], workspace: "" },
          { ...fixtureSnapshot.nodes[1], workspace: "   " },
          { ...fixtureSnapshot.nodes[2], workspace: 42 },
        ],
      },
    };
    const parsed = parseSnapshotResponse(wire);
    expect(parsed?.nodes.map((n) => n.workspace)).toEqual([
      undefined,
      undefined,
      undefined,
    ]);
  });

  it("preserves finite numeric x/y coordinates when present", () => {
    const wire = {
      snapshot: {
        ...fixtureSnapshot,
        nodes: [
          { ...fixtureSnapshot.nodes[0], x: 12.5, y: -7.25 },
          { ...fixtureSnapshot.nodes[1], x: 0, y: 0 }, // 0 is finite
        ],
      },
    };
    const parsed = parseSnapshotResponse(wire);
    expect(parsed?.nodes[0]?.x).toBe(12.5);
    expect(parsed?.nodes[0]?.y).toBe(-7.25);
    expect(parsed?.nodes[1]?.x).toBe(0);
    expect(parsed?.nodes[1]?.y).toBe(0);
  });

  it("coerces invalid x/y to undefined so hasPrecomputedCoordinates degrades safely", () => {
    const wire = {
      snapshot: {
        ...fixtureSnapshot,
        nodes: [
          { ...fixtureSnapshot.nodes[0], x: null, y: "left" },
          { ...fixtureSnapshot.nodes[1], x: Number.NaN, y: Number.POSITIVE_INFINITY },
          { ...fixtureSnapshot.nodes[2], x: Number.NEGATIVE_INFINITY, y: null },
          { ...fixtureSnapshot.nodes[3] }, // missing
        ],
      },
    };
    const parsed = parseSnapshotResponse(wire);
    // Every coord on every node is non-finite or missing, so all four
    // (x, y) pairs must be undefined.
    expect(parsed?.nodes.map((n) => [n.x, n.y])).toEqual([
      [undefined, undefined],
      [undefined, undefined],
      [undefined, undefined],
      [undefined, undefined],
    ]);
  });

  it("treats zero coordinates as finite, not as missing", () => {
    const wire = {
      snapshot: {
        ...fixtureSnapshot,
        nodes: [{ ...fixtureSnapshot.nodes[0], x: 0, y: -0 }],
      },
    };
    const parsed = parseSnapshotResponse(wire);
    // Object.is(-0, 0) is false, but Number.isFinite(-0) is true and
    // the snapshot path treats 0 / -0 as a valid origin position, not
    // as a missing field. We assert the post-parse values are numbers
    // and that hasPrecomputedCoordinates flips to true.
    expect(typeof parsed?.nodes[0]?.x).toBe("number");
    expect(typeof parsed?.nodes[0]?.y).toBe("number");
    expect(parsed && hasPrecomputedCoordinates(parsed)).toBe(true);
  });
});

describe("prettifyLabel", () => {
  it("passes through plain identifiers", () => {
    expect(prettifyLabel("Client")).toBe("Client");
    expect(prettifyLabel("internal/repository/jobs.go")).toBe(
      "internal/repository/jobs.go",
    );
  });

  it("strips a SCIP type descriptor down to the type name", () => {
    expect(
      prettifyLabel(
        "scip-go gomod github.com/golang/go/src . context/Context#",
      ),
    ).toBe("Context");
  });

  it("strips a SCIP method descriptor and keeps the parens", () => {
    expect(
      prettifyLabel(
        "scip-go gomod github.com/golang/go/src . fmt/Errorf().",
      ),
    ).toBe("Errorf()");
  });

  it("handles backticked package paths", () => {
    expect(
      prettifyLabel(
        "scip-go gomod github.com/google/uuid v1.6.0 `github.com/google/uuid`/UUID#",
      ),
    ).toBe("UUID");
  });

  it("returns the original on any parse mismatch", () => {
    expect(prettifyLabel("")).toBe("");
    // Trailing whitespace is preserved by `split` — fine, this code path only
    // fires on real SCIP descriptors which always carry a non-empty descriptor.
    expect(prettifyLabel("scip-go ")).toBe("scip-go ");
  });
});

describe("filterSnapshotForWorkspace", () => {
  const workspaceSnapshot: SnapshotPayload = {
    ...fixtureSnapshot,
    total_nodes: 6,
    total_edges: 5,
    nodes: [
      {
        id: "api:file",
        kind: "file",
        label: "api.ts",
        pagerank: 0.5,
        workspace: "api",
      },
      {
        id: "api:fn",
        kind: "symbol",
        label: "apiFn",
        symbol_kind: "function",
        pagerank: 0.4,
        workspace: "api",
      },
      {
        id: "web:file",
        kind: "file",
        label: "web.ts",
        pagerank: 0.3,
        workspace: "web",
      },
      {
        id: "web:fn",
        kind: "symbol",
        label: "webFn",
        symbol_kind: "function",
        pagerank: 0.2,
        workspace: "web",
      },
      {
        id: "worker:file",
        kind: "file",
        label: "worker.ts",
        pagerank: 0.1,
        workspace: "worker",
      },
      {
        id: "worker:fn",
        kind: "symbol",
        label: "workerFn",
        symbol_kind: "function",
        pagerank: 0.1,
        workspace: "worker",
      },
    ],
    edges: [
      {
        from: "api:file",
        to: "api:fn",
        kind: "ContainsDefinition",
        confidence: 1,
      },
      {
        from: "api:fn",
        to: "web:fn",
        kind: "SymbolReference",
        confidence: 0.8,
      },
      {
        from: "web:file",
        to: "web:fn",
        kind: "ContainsDefinition",
        confidence: 1,
      },
      {
        from: "worker:file",
        to: "worker:fn",
        kind: "ContainsDefinition",
        confidence: 1,
      },
      {
        from: "web:fn",
        to: "worker:fn",
        kind: "SymbolReference",
        confidence: 0.7,
      },
    ],
  };

  it("returns the original full snapshot for All", () => {
    expect(filterSnapshotForWorkspace(workspaceSnapshot, null)).toBe(
      workspaceSnapshot,
    );
    expect(filterSnapshotForWorkspace(workspaceSnapshot, "")).toBe(workspaceSnapshot);
  });

  it("keeps selected nodes plus remote endpoints for cross-workspace edges", () => {
    const filtered = filterSnapshotForWorkspace(workspaceSnapshot, "api");
    expect(filtered.nodes.map((node) => node.id)).toEqual([
      "api:file",
      "api:fn",
      "web:fn",
    ]);
    expect(filtered.edges.map((edge) => [edge.from, edge.to])).toEqual([
      ["api:file", "api:fn"],
      ["api:fn", "web:fn"],
    ]);
    expect(
      filtered.nodes.find((node) => node.id === "api:fn")?.workspace_context,
    ).toBeUndefined();
    expect(
      filtered.nodes.find((node) => node.id === "web:fn")?.workspace_context,
    ).toBe(true);
    expect(filtered.nodes.some((node) => node.id.startsWith("worker:"))).toBe(
      false,
    );
  });
});

describe("hasPrecomputedCoordinates", () => {
  it("returns false for a snapshot whose nodes lack any coordinates", () => {
    expect(hasPrecomputedCoordinates(fixtureSnapshot)).toBe(false);
  });

  it("returns false when even one node has a missing or invalid coordinate", () => {
    const partial: SnapshotPayload = {
      ...fixtureSnapshot,
      nodes: [
        { ...fixtureSnapshot.nodes[0], x: 1, y: 2 },
        { ...fixtureSnapshot.nodes[1], x: 3, y: 4 },
        { ...fixtureSnapshot.nodes[2] }, // missing
        { ...fixtureSnapshot.nodes[3], x: 5, y: 6 },
      ],
    };
    expect(hasPrecomputedCoordinates(partial)).toBe(false);
  });

  it("returns false for non-finite coordinate values (NaN, ±Infinity, strings)", () => {
    const wire = {
      snapshot: {
        ...fixtureSnapshot,
        nodes: [
          { ...fixtureSnapshot.nodes[0], x: Number.NaN, y: 1 },
          { ...fixtureSnapshot.nodes[1], x: 2, y: Number.POSITIVE_INFINITY },
          { ...fixtureSnapshot.nodes[2], x: 3, y: Number.NEGATIVE_INFINITY },
          { ...fixtureSnapshot.nodes[3], x: "left" as unknown as number, y: 4 },
        ],
      },
    };
    const parsed = parseSnapshotResponse(wire);
    expect(parsed).not.toBeNull();
    if (parsed) expect(hasPrecomputedCoordinates(parsed)).toBe(false);
  });

  it("returns true when every node has finite numeric coordinates", () => {
    const complete: SnapshotPayload = {
      ...fixtureSnapshot,
      nodes: fixtureSnapshot.nodes.map((n, i) => ({
        ...n,
        x: i * 10,
        y: -(i * 7),
      })),
    };
    expect(hasPrecomputedCoordinates(complete)).toBe(true);
  });

  it("returns true for an empty snapshot (vacuous: no nodes to verify)", () => {
    const empty: SnapshotPayload = {
      ...fixtureSnapshot,
      nodes: [],
      total_nodes: 0,
      edges: [],
      total_edges: 0,
    };
    expect(hasPrecomputedCoordinates(empty)).toBe(true);
  });

  it("treats 0 / -0 as a valid finite coordinate, not as missing", () => {
    const origin: SnapshotPayload = {
      ...fixtureSnapshot,
      nodes: fixtureSnapshot.nodes.map((n) => ({ ...n, x: 0, y: -0 })),
    };
    expect(hasPrecomputedCoordinates(origin)).toBe(true);
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

  it("carries workspace tags onto graphology node attributes", () => {
    const withWorkspace: SnapshotPayload = {
      ...fixtureSnapshot,
      nodes: fixtureSnapshot.nodes.map((n) =>
        n.id === "file:src/main.rs" ? { ...n, workspace: "app" } : n,
      ),
    };
    const graph = buildGraphFromSnapshot(withWorkspace);
    expect(graph.getNodeAttribute("file:src/main.rs", "workspace")).toBe(
      "app",
    );
    expect(graph.getNodeAttribute("file:src/main.rs", "workspaceColor")).toBe(
      colorForWorkspace("app"),
    );
    expect(graph.getNodeAttribute("file:src/main.rs", "workspaceBadge")).toBe(
      "A",
    );
    expect(graph.getNodeAttribute("file:src/main.rs", "label")).toBe(
      "main.rs · app",
    );
    expect(graph.getNodeAttribute("file:src/main.rs", "baseLabel")).toBe(
      "main.rs",
    );
    expect(graph.getNodeAttribute("file:src/main.rs", "borderColor")).toBe(
      colorForWorkspace("app"),
    );
  });

  it("carries workspace context markers onto graphology node attributes", () => {
    const withWorkspaceContext: SnapshotPayload = {
      ...fixtureSnapshot,
      nodes: fixtureSnapshot.nodes.map((n) =>
        n.id === "symbol:scip-rust . . . User#"
          ? { ...n, workspace_context: true }
          : n,
      ),
    };
    const graph = buildGraphFromSnapshot(withWorkspaceContext);
    expect(
      graph.getNodeAttribute("symbol:scip-rust . . . User#", "isWorkspaceContext"),
    ).toBe(true);
    expect(
      graph.getNodeAttribute("file:src/main.rs", "isWorkspaceContext"),
    ).toBe(false);
  });

  it("attaches endpoint workspace metadata to edges", () => {
    const withWorkspace: SnapshotPayload = {
      ...fixtureSnapshot,
      nodes: fixtureSnapshot.nodes.map((n) => {
        if (n.id === "symbol:scip-rust . . . main()") {
          return { ...n, workspace: "app" };
        }
        if (n.id === "symbol:scip-rust . . . User#") {
          return { ...n, workspace: "domain" };
        }
        return n;
      }),
    };
    const graph = buildGraphFromSnapshot(withWorkspace);
    const crossEdge = graph
      .edges()
      .find(
        (edge) =>
          graph.source(edge) === "symbol:scip-rust . . . main()" &&
          graph.target(edge) === "symbol:scip-rust . . . User#",
      );
    expect(crossEdge).toBeDefined();
    expect(graph.getEdgeAttribute(crossEdge!, "sourceWorkspace")).toBe("app");
    expect(graph.getEdgeAttribute(crossEdge!, "targetWorkspace")).toBe(
      "domain",
    );
    expect(graph.getEdgeAttribute(crossEdge!, "isCrossWorkspace")).toBe(true);
    expect(graph.getEdgeAttribute(crossEdge!, "crossWorkspace")).toBe(true);
    expect(graph.getEdgeAttribute(crossEdge!, "color")).toBe("#facc15");
    expect(graph.getEdgeAttribute(crossEdge!, "zIndex")).toBe(20);
    expect(graph.getEdgeAttribute(crossEdge!, "lineStyle")).toBe("dashed");
  });

  it("keeps intra-workspace edges less prominent than cross-workspace edges", () => {
    const withWorkspace: SnapshotPayload = {
      ...fixtureSnapshot,
      nodes: fixtureSnapshot.nodes.map((n) => {
        if (n.id === "file:src/main.rs" || n.id === "symbol:scip-rust . . . main()") {
          return { ...n, workspace: "app" };
        }
        if (n.id === "symbol:scip-rust . . . User#") {
          return { ...n, workspace: "domain" };
        }
        return n;
      }),
    };
    const graph = buildGraphFromSnapshot(withWorkspace);
    const intraEdge = graph.edges().find(
      (edge) =>
        graph.source(edge) === "file:src/main.rs" &&
        graph.target(edge) === "symbol:scip-rust . . . main()",
    );
    const crossEdge = graph.edges().find(
      (edge) =>
        graph.source(edge) === "symbol:scip-rust . . . main()" &&
        graph.target(edge) === "symbol:scip-rust . . . User#",
    );
    expect(intraEdge).toBeDefined();
    expect(crossEdge).toBeDefined();
    expect(graph.getEdgeAttribute(intraEdge!, "isCrossWorkspace")).toBe(false);
    expect(graph.getEdgeAttribute(crossEdge!, "isCrossWorkspace")).toBe(true);
    expect(graph.getEdgeAttribute(crossEdge!, "size") as number).toBeGreaterThan(
      graph.getEdgeAttribute(intraEdge!, "size") as number,
    );
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

  // ── Precomputed-coordinate path (server-shipped layout) ───────────────────

  const withServerCoords = (): SnapshotPayload => ({
    ...fixtureSnapshot,
    nodes: [
      { ...fixtureSnapshot.nodes[0], x: 12.5, y: -7.25 },
      { ...fixtureSnapshot.nodes[1], x: 100, y: 200 },
      { ...fixtureSnapshot.nodes[2], x: -50, y: 0 },
      { ...fixtureSnapshot.nodes[3], x: 0.001, y: -0.001 },
    ],
  });

  it("uses server-provided positions verbatim when every node has finite x/y", () => {
    const graph = buildGraphFromSnapshot(withServerCoords());
    expect(graph.getNodeAttribute("file:src/main.rs", "x")).toBe(12.5);
    expect(graph.getNodeAttribute("file:src/main.rs", "y")).toBe(-7.25);
    expect(
      graph.getNodeAttribute("symbol:scip-rust . . . main()", "x"),
    ).toBe(100);
    expect(
      graph.getNodeAttribute("symbol:scip-rust . . . main()", "y"),
    ).toBe(200);
    expect(
      graph.getNodeAttribute("symbol:scip-rust . . . User#", "x"),
    ).toBe(-50);
    expect(
      graph.getNodeAttribute("symbol:scip-rust . . . User#", "y"),
    ).toBe(0);
    expect(graph.getNodeAttribute("file:src/user.rs", "x")).toBe(0.001);
    expect(graph.getNodeAttribute("file:src/user.rs", "y")).toBe(-0.001);
  });

  it("marks the graph with the precomputedLayout attribute so useSigmaGraph can skip FA2", () => {
    const graph = buildGraphFromSnapshot(withServerCoords());
    expect(graph.getAttribute(PRECOMPUTED_LAYOUT_ATTRIBUTE)).toBe(true);
  });

  it("does not mark the graph as precomputed when coordinates are incomplete", () => {
    const partial: SnapshotPayload = {
      ...fixtureSnapshot,
      nodes: [
        { ...fixtureSnapshot.nodes[0], x: 1, y: 2 },
        { ...fixtureSnapshot.nodes[1] }, // missing
        { ...fixtureSnapshot.nodes[2] },
        { ...fixtureSnapshot.nodes[3] },
      ],
    };
    const graph = buildGraphFromSnapshot(partial);
    expect(graph.getAttribute(PRECOMPUTED_LAYOUT_ATTRIBUTE)).toBeUndefined();
  });

  it("does not mark the graph as precomputed when no node has coordinates (existing seed path)", () => {
    const graph = buildGraphFromSnapshot(fixtureSnapshot);
    expect(graph.getAttribute(PRECOMPUTED_LAYOUT_ATTRIBUTE)).toBeUndefined();
  });

  it("falls back to the seed path when parseSnapshotResponse drops a single invalid coord", () => {
    // Wire payload deliberately ships one NaN; after parseSnapshotResponse
    // the parsed node has x/y = undefined, so the helper flips to false and
    // the function must take the seed branch.
    const wire = {
      snapshot: {
        ...fixtureSnapshot,
        nodes: [
          { ...fixtureSnapshot.nodes[0], x: 1, y: 2 },
          { ...fixtureSnapshot.nodes[1], x: 3, y: 4 },
          { ...fixtureSnapshot.nodes[2], x: Number.NaN, y: 5 },
          { ...fixtureSnapshot.nodes[3], x: 6, y: 7 },
        ],
      },
    };
    const parsed = parseSnapshotResponse(wire);
    expect(parsed).not.toBeNull();
    if (!parsed) return;
    const graph = buildGraphFromSnapshot(parsed);
    expect(graph.getAttribute(PRECOMPUTED_LAYOUT_ATTRIBUTE)).toBeUndefined();
  });

  it("still wires up edges when using the precomputed-coordinate path", () => {
    const graph = buildGraphFromSnapshot(withServerCoords());
    expect(graph.order).toBe(fixtureSnapshot.nodes.length);
    expect(graph.size).toBe(fixtureSnapshot.edges.length);
  });

  it("treats (0, 0) as a valid precomputed position rather than falling back", () => {
    const atOrigin: SnapshotPayload = {
      ...fixtureSnapshot,
      nodes: fixtureSnapshot.nodes.map((n) => ({ ...n, x: 0, y: 0 })),
    };
    const graph = buildGraphFromSnapshot(atOrigin);
    expect(graph.getAttribute(PRECOMPUTED_LAYOUT_ATTRIBUTE)).toBe(true);
    for (const id of graph.nodes()) {
      expect(graph.getNodeAttribute(id, "x")).toBe(0);
      expect(graph.getNodeAttribute(id, "y")).toBe(0);
    }
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

  it("falls back to parent-directory hash when community_id is absent", () => {
    const sym: SnapshotNode = {
      id: "s",
      kind: "symbol",
      label: "x",
      pagerank: 0,
      symbol_kind: "function",
      file_path: "server/crates/djinn-graph/src/lib.rs",
    };
    expect(colorForNode(sym)).toBe(
      colorForCommunity("server/crates/djinn-graph/src"),
    );
  });

  it("colors files by parent-directory hash so siblings share a hue", () => {
    const a: SnapshotNode = {
      id: "f1",
      kind: "file",
      label: "page_worker.go",
      file_path: "internal/worker/page_worker.go",
      pagerank: 0,
    };
    const b: SnapshotNode = {
      id: "f2",
      kind: "file",
      label: "interfaces.go",
      file_path: "internal/worker/interfaces.go",
      pagerank: 0,
    };
    const c: SnapshotNode = {
      id: "f3",
      kind: "file",
      label: "client.go",
      file_path: "internal/strategies/connectwise/client.go",
      pagerank: 0,
    };
    expect(colorForNode(a)).toBe(colorForCommunity("internal/worker"));
    expect(colorForNode(a)).toBe(colorForNode(b));
    expect(colorForNode(a)).not.toBe(colorForNode(c));
  });

  it("uses the project accent for folders that look like the project root", () => {
    expect(
      colorForNode({
        id: "p",
        kind: "folder",
        label: "Project",
        pagerank: 0,
      }),
    ).toBe("#a855f7");
  });

  it("colorForCommunity is deterministic across calls", () => {
    expect(colorForCommunity("alpha")).toBe(colorForCommunity("alpha"));
  });

  it("colorForWorkspace is deterministic and separate from topology color", () => {
    expect(colorForWorkspace("api")).toBe(colorForWorkspace("api"));
    expect(WORKSPACE_COLORS).toContain(colorForWorkspace("api"));
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
