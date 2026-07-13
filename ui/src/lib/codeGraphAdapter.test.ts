import { describe, expect, it } from "vitest";
import {
  COMMUNITY_COLORS,
  COMMUNITY_HULLS_ATTRIBUTE,
  CONTAINMENT_EDGE_KINDS,
  DOUBLE_CLICK_INTERVAL_MS,
  CURVED_EDGE_TYPE,
  LOD_FAR_RATIO,
  LOD_MID_RATIO,
  MAX_RENDERED_EDGES,
  PRECOMPUTED_LAYOUT_ATTRIBUTE,
  STRAIGHT_EDGE_THRESHOLD,
  STRAIGHT_EDGE_TYPE,
  readEdgeRenderStats,
  VIEWPORT_CULLING_MARGIN,
  VIEWPORT_CULLING_THRESHOLD,
  WORKSPACE_COLORS,
  buildGraphFromSnapshot,
  colorForCommunity,
  colorForGroup,
  computeCrateLegend,
  colorForNode,
  colorForWorkspace,
  crateForPath,
  GROUP_COLORS,
  deriveCommunityHulls,
  selectRenderableHulls,
  MIN_HULL_MEMBERS,
  MAX_HULL_REGIONS,
  edgeStyleFor,
  filterSnapshotForWorkspace,
  hasPrecomputedCoordinates,
  isContainmentEdgeKind,
  isDoubleClick,
  isSymbolVisibleAtMidTier,
  isInViewport,
  lodTierForZoom,
  massForNode,
  parseSnapshotResponse,
  prettifyLabel,
  viewportBoundsEqual,
  type CommunityHull,
  type SnapshotNode,
  type SnapshotPayload,
  type ViewportBounds,
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
        nodes: [
          ...fixtureSnapshot.nodes,
          { id: "", kind: "file", label: "", pagerank: 0 },
        ],
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
          {
            ...fixtureSnapshot.nodes[2],
            cognitive: "huge" as unknown as number,
          },
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

  it("preserves kind: 'community' instead of coercing to symbol", () => {
    const wire = {
      snapshot: {
        ...fixtureSnapshot,
        nodes: [
          {
            id: "community:abc123",
            kind: "community",
            label: "auth-module",
            pagerank: 0.9,
            community_id: "abc123",
            member_count: 42,
            internal_edge_count: 17,
            workspace_kind: "single",
          },
        ],
      },
    };
    const parsed = parseSnapshotResponse(wire);
    expect(parsed?.nodes).toHaveLength(1);
    const node = parsed?.nodes[0];
    expect(node?.kind).toBe("community");
    expect(node?.community_id).toBe("abc123");
  });

  it("parses community metadata (member_count, internal_edge_count, workspace_kind)", () => {
    const wire = {
      snapshot: {
        ...fixtureSnapshot,
        nodes: [
          {
            id: "community:abc123",
            kind: "community",
            label: "auth-module",
            pagerank: 0.9,
            member_count: 128,
            internal_edge_count: 512,
            workspace_kind: "mixed",
          },
        ],
      },
    };
    const parsed = parseSnapshotResponse(wire);
    const node = parsed?.nodes[0];
    expect(node?.member_count).toBe(128);
    expect(node?.internal_edge_count).toBe(512);
    expect(node?.workspace_kind).toBe("mixed");
  });

  it("parses non-empty community keywords and drops blank legacy values", () => {
    const wire = {
      snapshot: {
        ...fixtureSnapshot,
        nodes: [
          {
            id: "community:abc123",
            kind: "community",
            label: "auth-module",
            pagerank: 0.9,
            keywords: ["auth", "  tokens  ", "", "   ", 42],
          },
          {
            id: "community:legacy",
            kind: "community",
            label: "legacy-module",
            pagerank: 0.1,
            keywords: [],
          },
        ],
      },
    };
    const parsed = parseSnapshotResponse(wire);
    expect(parsed?.nodes[0]?.keywords).toEqual(["auth", "tokens"]);
    expect(parsed?.nodes[1]?.keywords).toBeUndefined();
  });

  it("treats non-numeric / negative member_count as undefined", () => {
    const wire = {
      snapshot: {
        ...fixtureSnapshot,
        nodes: [
          {
            id: "community:a",
            kind: "community",
            label: "a",
            pagerank: 0,
            member_count: "lots" as unknown as number,
          },
          {
            id: "community:b",
            kind: "community",
            label: "b",
            pagerank: 0,
            member_count: -5,
          },
          {
            id: "community:c",
            kind: "community",
            label: "c",
            pagerank: 0,
            internal_edge_count: Number.NaN,
          },
        ],
      },
    };
    const parsed = parseSnapshotResponse(wire);
    expect(parsed?.nodes[0]?.member_count).toBeUndefined();
    expect(parsed?.nodes[1]?.member_count).toBeUndefined();
    expect(parsed?.nodes[2]?.internal_edge_count).toBeUndefined();
  });

  it("leaves community metadata undefined for symbol/file/folder nodes", () => {
    const wire = { snapshot: fixtureSnapshot };
    const parsed = parseSnapshotResponse(wire);
    for (const node of parsed?.nodes ?? []) {
      expect(node.member_count).toBeUndefined();
      expect(node.internal_edge_count).toBeUndefined();
      expect(node.workspace_kind).toBeUndefined();
    }
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
          {
            ...fixtureSnapshot.nodes[1],
            x: Number.NaN,
            y: Number.POSITIVE_INFINITY,
          },
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
      prettifyLabel("scip-go gomod github.com/golang/go/src . fmt/Errorf()."),
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
    expect(filterSnapshotForWorkspace(workspaceSnapshot, "")).toBe(
      workspaceSnapshot,
    );
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

describe("buildGraphFromSnapshot (file complexity)", () => {
  it("stamps file nodes with their worst function's cognitive", () => {
    const snapshot: SnapshotPayload = {
      project_id: "p",
      git_head: "h",
      generated_at: "2026-04-28T00:00:00Z",
      truncated: false,
      total_nodes: 4,
      total_edges: 0,
      nodes: [
        {
          id: "file:src/a.rs",
          kind: "file",
          label: "a.rs",
          file_path: "src/a.rs",
          pagerank: 0.5,
          x: 0,
          y: 0,
        },
        {
          id: "symbol:f1",
          kind: "symbol",
          label: "f1",
          symbol_kind: "function",
          file_path: "src/a.rs",
          pagerank: 0.1,
          cognitive: 4,
          x: 1,
          y: 1,
        },
        {
          id: "symbol:f2",
          kind: "symbol",
          label: "f2",
          symbol_kind: "function",
          file_path: "src/a.rs",
          pagerank: 0.1,
          cognitive: 11,
          x: 2,
          y: 2,
        },
        {
          id: "file:src/empty.rs",
          kind: "file",
          label: "empty.rs",
          file_path: "src/empty.rs",
          pagerank: 0.2,
          x: 3,
          y: 3,
        },
      ],
      edges: [],
    };
    const graph = buildGraphFromSnapshot(snapshot);
    // File with functions → max cognitive (11, not 4 or 15).
    expect(graph.getNodeAttribute("file:src/a.rs", "cognitive")).toBe(11);
    // File with no complexity-bearing symbols → no cognitive stamped.
    expect(
      graph.getNodeAttribute("file:src/empty.rs", "cognitive"),
    ).toBeUndefined();
  });
});

describe("buildGraphFromSnapshot", () => {
  it("emits one graphology node per snapshot node, excluding containment edges", () => {
    const graph = buildGraphFromSnapshot(fixtureSnapshot);
    expect(graph.order).toBe(fixtureSnapshot.nodes.length);
    // ContainsDefinition / DeclaredInFile / MemberOf are containment
    // edges — they are never rendered as Sigma edges. The fixture has
    // 2 ContainsDefinition edges + 1 SymbolReference edge, so only the
    // SymbolReference survives.
    const nonContainmentEdges = fixtureSnapshot.edges.filter(
      (e) => !isContainmentEdgeKind(e.kind),
    ).length;
    expect(graph.size).toBe(nonContainmentEdges);
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
    expect(graph.getNodeAttribute("file:src/main.rs", "workspace")).toBe("app");
    expect(graph.getNodeAttribute("file:src/main.rs", "workspaceColor")).toBe(
      colorForWorkspace("app"),
    );
    expect(graph.getNodeAttribute("file:src/main.rs", "workspaceBadge")).toBe(
      "A",
    );
    expect(graph.getNodeAttribute("file:src/main.rs", "label")).toBe("main.rs");
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
          ? { ...n, workspace: "domain", workspace_context: true }
          : n,
      ),
    };
    const graph = buildGraphFromSnapshot(withWorkspaceContext);
    expect(
      graph.getNodeAttribute(
        "symbol:scip-rust . . . User#",
        "isWorkspaceContext",
      ),
    ).toBe(true);
    expect(
      graph.getNodeAttribute("file:src/main.rs", "isWorkspaceContext"),
    ).toBe(false);
    expect(
      graph.getNodeAttribute("symbol:scip-rust . . . User#", "label"),
    ).toBe("User · domain");
  });

  it("does not append workspace suffix to selected nodes when filtering for a single workspace", () => {
    const wsSnapshot: SnapshotPayload = {
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
    const filtered = filterSnapshotForWorkspace(wsSnapshot, "api");
    const graph = buildGraphFromSnapshot(filtered);
    // Selected workspace nodes should NOT have the "· {workspace}" suffix.
    expect(graph.getNodeAttribute("api:file", "label")).toBe("api.ts");
    expect(graph.getNodeAttribute("api:fn", "label")).toBe("apiFn");
    // Remote context nodes SHOULD still have the suffix.
    expect(graph.getNodeAttribute("web:fn", "label")).toBe("webFn · web");
  });

  it("folds legacy community keywords into hull metadata, not node labels", () => {
    const communitySnapshot: SnapshotPayload = {
      ...fixtureSnapshot,
      total_nodes: 3,
      total_edges: 0,
      nodes: [
        {
          id: "community:auth",
          kind: "community",
          label: "auth",
          pagerank: 0.8,
          community_id: "auth",
          keywords: ["tokens", "sessions"],
        },
        {
          id: "community:legacy",
          kind: "community",
          label: "legacy",
          pagerank: 0.2,
        },
        {
          id: "file:src/main.rs",
          kind: "file",
          label: "main.rs",
          pagerank: 0.1,
          keywords: ["ignored"],
        },
      ],
      edges: [],
    };
    const graph = buildGraphFromSnapshot(communitySnapshot);
    // Community entries are not visible nodes.
    expect(graph.hasNode("community:auth")).toBe(false);
    expect(graph.hasNode("community:legacy")).toBe(false);
    // The file node is still present without a subtitle.
    expect(graph.getNodeAttribute("file:src/main.rs", "label")).toBe("main.rs");
    expect(
      graph.getNodeAttribute("file:src/main.rs", "subtitle"),
    ).toBeUndefined();
    // Hull metadata carries the legacy label + keywords.
    const hulls = graph.getAttribute(
      COMMUNITY_HULLS_ATTRIBUTE,
    ) as CommunityHull[];
    const authHull = hulls.find((h) => h.id === "auth");
    expect(authHull).toBeDefined();
    expect(authHull?.label).toBe("auth");
    expect(authHull?.keywords).toEqual(["tokens", "sessions"]);
    const legacyHull = hulls.find((h) => h.id === "community:legacy");
    expect(legacyHull).toBeDefined();
    expect(legacyHull?.label).toBe("legacy");
    expect(legacyHull?.keywords).toBeUndefined();
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
    // Add a non-containment edge (Reads) so the test has a valid rendered
    // intra-workspace edge to compare. ContainsDefinition is excluded
    // from rendered edges as containment nesting metadata.
    const baseEdges = [
      ...fixtureSnapshot.edges,
      {
        from: "symbol:scip-rust . . . main()",
        to: "symbol:scip-rust . . . User#",
        kind: "Reads",
        confidence: 0.8,
      },
    ];
    // Intra-workspace: both symbols in workspace "app".
    const intraSnapshot: SnapshotPayload = {
      ...fixtureSnapshot,
      edges: baseEdges,
      nodes: fixtureSnapshot.nodes.map((n) => {
        if (
          n.id === "file:src/main.rs" ||
          n.id === "symbol:scip-rust . . . main()" ||
          n.id === "symbol:scip-rust . . . User#"
        ) {
          return { ...n, workspace: "app" };
        }
        return n;
      }),
    };
    // Cross-workspace: main() in "app", User# in "domain".
    const crossSnapshot: SnapshotPayload = {
      ...fixtureSnapshot,
      edges: baseEdges,
      nodes: fixtureSnapshot.nodes.map((n) => {
        if (
          n.id === "file:src/main.rs" ||
          n.id === "symbol:scip-rust . . . main()"
        ) {
          return { ...n, workspace: "app" };
        }
        if (n.id === "symbol:scip-rust . . . User#") {
          return { ...n, workspace: "domain" };
        }
        return n;
      }),
    };
    const intraGraph = buildGraphFromSnapshot(intraSnapshot);
    const crossGraph = buildGraphFromSnapshot(crossSnapshot);
    const findReadsEdge = (g: ReturnType<typeof buildGraphFromSnapshot>) =>
      g.edges().find(
        (edge) =>
          g.getEdgeAttribute(edge, "kind") === "Reads" &&
          g.source(edge) === "symbol:scip-rust . . . main()" &&
          g.target(edge) === "symbol:scip-rust . . . User#",
      );
    const intraEdge = findReadsEdge(intraGraph);
    const crossEdge = findReadsEdge(crossGraph);
    expect(intraEdge).toBeDefined();
    expect(crossEdge).toBeDefined();
    expect(
      intraGraph.getEdgeAttribute(intraEdge!, "isCrossWorkspace"),
    ).toBe(false);
    expect(
      crossGraph.getEdgeAttribute(crossEdge!, "isCrossWorkspace"),
    ).toBe(true);
    expect(
      crossGraph.getEdgeAttribute(crossEdge!, "size") as number,
    ).toBeGreaterThan(
      intraGraph.getEdgeAttribute(intraEdge!, "size") as number,
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
    const a = graph.getNodeAttributes(
      "symbol:scip-rust . . . main()",
    ) as Record<string, unknown>;
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
    // Only non-containment, non-self-loop edges survive. The loop is a
    // FileReference (non-containment) but gets dropped by dropSelfLoops.
    const expected = fixtureSnapshot.edges.filter(
      (e) => !isContainmentEdgeKind(e.kind),
    ).length;
    expect(graph.size).toBe(expected);
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
    // Only non-containment edges with valid endpoints survive.
    const expected = fixtureSnapshot.edges.filter(
      (e) => !isContainmentEdgeKind(e.kind),
    ).length;
    expect(graph.size).toBe(expected);
  });

  it("paints edges with the per-kind color", () => {
    const graph = buildGraphFromSnapshot(fixtureSnapshot);
    // The fixture's only rendered edge is a SymbolReference.
    const symbolRefEdges = graph
      .edges()
      .filter(
        (e) => graph.getEdgeAttribute(e, "kind") === "SymbolReference",
      );
    expect(symbolRefEdges.length).toBeGreaterThan(0);
    for (const e of symbolRefEdges) {
      expect(graph.getEdgeAttribute(e, "color")).toBe("#7c3aed");
    }
  });

  it("excludes all containment edge kinds from rendered graph edges", () => {
    const withContainment: SnapshotPayload = {
      ...fixtureSnapshot,
      edges: [
        ...fixtureSnapshot.edges,
        // Add one of each containment kind so the test exercises all three.
        {
          from: "symbol:scip-rust . . . User#",
          to: "file:src/user.rs",
          kind: "MemberOf",
          confidence: 1.0,
        },
        {
          from: "file:src/user.rs",
          to: "symbol:scip-rust . . . User#",
          kind: "DeclaredInFile",
          confidence: 1.0,
        },
      ],
    };
    const graph = buildGraphFromSnapshot(withContainment);
    // Every rendered edge must be a non-containment kind.
    for (const e of graph.edges()) {
      const kind = graph.getEdgeAttribute(e, "kind") as string;
      expect(isContainmentEdgeKind(kind)).toBe(false);
    }
    // The only surviving edge is the fixture's SymbolReference.
    const expected = withContainment.edges.filter(
      (e) => !isContainmentEdgeKind(e.kind),
    ).length;
    expect(graph.size).toBe(expected);
    expect(graph.size).toBe(1);
  });

  it("converts containment edges into nesting metadata on nodes", () => {
    const graph = buildGraphFromSnapshot(fixtureSnapshot);
    // ContainsDefinition: file:src/main.rs → symbol:main()
    expect(
      graph.getNodeAttribute("symbol:scip-rust . . . main()", "parentId"),
    ).toBe("file:src/main.rs");
    // ContainsDefinition: file:src/user.rs → symbol:User#
    expect(
      graph.getNodeAttribute("symbol:scip-rust . . . User#", "parentId"),
    ).toBe("file:src/user.rs");
    // The parent file carries its children in childIds.
    const mainChildren = graph.getNodeAttribute(
      "file:src/main.rs",
      "childIds",
    ) as string[] | undefined;
    expect(mainChildren).toContain("symbol:scip-rust . . . main()");
    const userChildren = graph.getNodeAttribute(
      "file:src/user.rs",
      "childIds",
    ) as string[] | undefined;
    expect(userChildren).toContain("symbol:scip-rust . . . User#");
  });

  it("converts DeclaredInFile and MemberOf back-edges into parent nesting", () => {
    const containmentSnapshot: SnapshotPayload = {
      ...fixtureSnapshot,
      nodes: [
        ...fixtureSnapshot.nodes,
        {
          id: "symbol:scip-rust . . . User#name.",
          kind: "symbol",
          label: "name",
          symbol_kind: "field",
          file_path: "src/user.rs",
          pagerank: 0.1,
        },
      ],
      edges: [
        {
          from: "symbol:scip-rust . . . main()",
          to: "file:src/main.rs",
          kind: "DeclaredInFile",
          confidence: 1,
        },
        {
          from: "symbol:scip-rust . . . User#name.",
          to: "symbol:scip-rust . . . User#",
          kind: "MemberOf",
          confidence: 1,
        },
      ],
    };
    const graph = buildGraphFromSnapshot(containmentSnapshot);
    expect(
      graph.getNodeAttribute("symbol:scip-rust . . . main()", "parentId"),
    ).toBe("file:src/main.rs");
    expect(
      graph.getNodeAttribute("symbol:scip-rust . . . User#name.", "parentId"),
    ).toBe("symbol:scip-rust . . . User#");
    expect(
      graph.getNodeAttribute("file:src/main.rs", "childIds"),
    ).toContain("symbol:scip-rust . . . main()");
    expect(
      graph.getNodeAttribute("symbol:scip-rust . . . User#", "childIds"),
    ).toContain("symbol:scip-rust . . . User#name.");
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
    expect(graph.getNodeAttribute("symbol:scip-rust . . . main()", "x")).toBe(
      100,
    );
    expect(graph.getNodeAttribute("symbol:scip-rust . . . main()", "y")).toBe(
      200,
    );
    expect(graph.getNodeAttribute("symbol:scip-rust . . . User#", "x")).toBe(
      -50,
    );
    expect(graph.getNodeAttribute("symbol:scip-rust . . . User#", "y")).toBe(0);
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
    // Containment edges are excluded even on the precomputed path.
    const expected = fixtureSnapshot.edges.filter(
      (e) => !isContainmentEdgeKind(e.kind),
    ).length;
    expect(graph.size).toBe(expected);
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

  it("produces identical positions across two consecutive calls (determinism)", () => {
    // The adapter delegates to computeForceLayout (or sequential/radial)
    // which uses deterministic hash-based jitter — no Math.random().
    // Two calls on the same snapshot must yield bitwise-identical x/y.
    const graph1 = buildGraphFromSnapshot(fixtureSnapshot);
    const graph2 = buildGraphFromSnapshot(fixtureSnapshot);

    expect(graph1.order).toBe(graph2.order);
    for (const id of graph1.nodes()) {
      expect(graph2.hasNode(id)).toBe(true);
      const x1 = graph1.getNodeAttribute(id, "x");
      const y1 = graph1.getNodeAttribute(id, "y");
      const x2 = graph2.getNodeAttribute(id, "x");
      const y2 = graph2.getNodeAttribute(id, "y");
      expect(x1).toBe(x2);
      expect(y1).toBe(y2);
    }
  });

  it("produces identical positions across calls (determinism)", () => {
    const graph1 = buildGraphFromSnapshot(fixtureSnapshot);
    const graph2 = buildGraphFromSnapshot(fixtureSnapshot);
    for (const id of graph1.nodes()) {
      expect(graph1.getNodeAttribute(id, "x")).toBe(
        graph2.getNodeAttribute(id, "x"),
      );
      expect(graph1.getNodeAttribute(id, "y")).toBe(
        graph2.getNodeAttribute(id, "y"),
      );
    }
  });
});

describe("edge salience cap", () => {
  // Build a dense snapshot: N symbol nodes with finite coordinates (so the
  // precomputed branch is taken — fast, no layout) and `edgeCount`
  // SymbolReference edges spread across them. `multi: true` lets duplicate
  // endpoints coexist, so we can exceed the cap with a modest node count.
  function denseSnapshot(
    nodeCount: number,
    edgeCount: number,
    confidenceAt: (i: number) => number = () => 1,
  ): SnapshotPayload {
    const nodes: SnapshotNode[] = Array.from({ length: nodeCount }, (_, i) => ({
      id: `symbol:n${i}`,
      kind: "symbol",
      label: `n${i}`,
      symbol_kind: "function",
      pagerank: 0.1,
      x: i,
      y: i,
    }));
    const edges = Array.from({ length: edgeCount }, (_, i) => ({
      from: `symbol:n${i % nodeCount}`,
      to: `symbol:n${(i + 1) % nodeCount}`,
      kind: "SymbolReference",
      confidence: confidenceAt(i),
      reason: undefined,
    }));
    return {
      project_id: "proj-dense",
      git_head: "dense",
      generated_at: "2026-04-28T00:00:00Z",
      truncated: false,
      total_nodes: nodeCount,
      total_edges: edgeCount,
      nodes,
      edges,
    };
  }

  it("caps rendered edges at MAX_RENDERED_EDGES and records the original total", () => {
    const total = MAX_RENDERED_EDGES + 500;
    const graph = buildGraphFromSnapshot(denseSnapshot(200, total));
    expect(graph.size).toBe(MAX_RENDERED_EDGES);
    expect(readEdgeRenderStats(graph)).toEqual({
      rendered: MAX_RENDERED_EDGES,
      total,
    });
  });

  it("does not trim when drawable edges sit under the ceiling", () => {
    const graph = buildGraphFromSnapshot(denseSnapshot(50, 100));
    expect(graph.size).toBe(100);
    expect(readEdgeRenderStats(graph)).toEqual({ rendered: 100, total: 100 });
  });

  it("keeps the highest-confidence edges when trimming", () => {
    // Last `MAX` edges get confidence 1, the rest 0.1 — only the
    // high-confidence tail should survive the salience sort.
    const total = MAX_RENDERED_EDGES + 1000;
    const graph = buildGraphFromSnapshot(
      denseSnapshot(200, total, (i) => (i >= total - MAX_RENDERED_EDGES ? 1 : 0.1)),
    );
    expect(graph.size).toBe(MAX_RENDERED_EDGES);
    let lowConfidenceKept = 0;
    graph.forEachEdge((_e, attrs) => {
      if ((attrs.confidence as number) < 0.5) lowConfidenceKept += 1;
    });
    expect(lowConfidenceKept).toBe(0);
  });

  it("draws straight edges once the rendered set is dense", () => {
    const graph = buildGraphFromSnapshot(
      denseSnapshot(200, STRAIGHT_EDGE_THRESHOLD + 50),
    );
    let straight = 0;
    let curved = 0;
    graph.forEachEdge((_e, attrs) => {
      if (attrs.type === STRAIGHT_EDGE_TYPE) straight += 1;
      if (attrs.type === CURVED_EDGE_TYPE) curved += 1;
    });
    // All intra-crate edges (no workspace tags in the fixture) go straight.
    expect(straight).toBe(STRAIGHT_EDGE_THRESHOLD + 50);
    expect(curved).toBe(0);
    // Straight edges carry no curvature attribute.
    graph.forEachEdge((_e, attrs) => {
      expect(attrs.curvature).toBeUndefined();
    });
  });

  it("keeps curved edges for sparse graphs", () => {
    const graph = buildGraphFromSnapshot(denseSnapshot(50, 100));
    graph.forEachEdge((_e, attrs) => {
      expect(attrs.type).toBe(CURVED_EDGE_TYPE);
      expect(attrs.curvature).toBeGreaterThan(0);
    });
  });

  it("keeps cross-crate edges curved even in a dense graph", () => {
    // Two crates; every edge crosses between them, so all stay curved
    // and dashed regardless of density.
    const n = 300;
    const edgeCount = STRAIGHT_EDGE_THRESHOLD + 100;
    const nodes: SnapshotNode[] = Array.from({ length: n }, (_, i) => ({
      id: `symbol:n${i}`,
      kind: "symbol",
      label: `n${i}`,
      symbol_kind: "function",
      pagerank: 0.1,
      x: i,
      y: i,
      workspace: i % 2 === 0 ? "crate-a" : "crate-b",
    }));
    const edges = Array.from({ length: edgeCount }, (_, i) => ({
      // even -> odd guarantees a crate-a -> crate-b crossing.
      from: `symbol:n${(i * 2) % n}`,
      to: `symbol:n${(i * 2 + 1) % n}`,
      kind: "SymbolReference",
      confidence: 1,
      reason: undefined,
    }));
    const graph = buildGraphFromSnapshot({
      project_id: "p",
      git_head: "g",
      generated_at: "2026-04-28T00:00:00Z",
      truncated: false,
      total_nodes: n,
      total_edges: edgeCount,
      nodes,
      edges,
    });
    let crossCurved = 0;
    graph.forEachEdge((_e, attrs) => {
      if (attrs.isCrossWorkspace === true) {
        expect(attrs.type).toBe(CURVED_EDGE_TYPE);
        expect(attrs.lineStyle).toBe("dashed");
        crossCurved += 1;
      }
    });
    expect(crossCurved).toBeGreaterThan(0);
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
  it("colors symbols by the crate derived from the path (crates/<name> wins over community)", () => {
    const sym: SnapshotNode = {
      id: "s",
      kind: "symbol",
      label: "x",
      pagerank: 0,
      symbol_kind: "function",
      community_id: "cluster-7",
      file_path: "server/crates/djinn-graph/src/lib.rs",
    };
    // Crate from the path is the most specific signal and wins.
    expect(colorForNode(sym)).toBe(colorForGroup("djinn-graph"));
  });

  it("uses the first path segment as the group for non-crates/ paths", () => {
    const sym: SnapshotNode = {
      id: "s",
      kind: "symbol",
      label: "x",
      pagerank: 0,
      symbol_kind: "function",
      file_path: "ui/src/lib/codeGraphAdapter.ts",
    };
    expect(colorForNode(sym)).toBe(colorForGroup("ui"));
  });

  it("colors every node in a crate the same hue, regardless of kind", () => {
    const sym: SnapshotNode = {
      id: "a",
      kind: "symbol",
      label: "a",
      pagerank: 0,
      symbol_kind: "function",
      file_path: "server/crates/djinn-graph/src/communities.rs",
    };
    const file: SnapshotNode = {
      id: "b",
      kind: "file",
      label: "lib.rs",
      file_path: "server/crates/djinn-graph/src/lib.rs",
      pagerank: 0,
    };
    expect(colorForNode(sym)).toBe(colorForNode(file));
    expect(colorForNode(sym)).toBe(colorForGroup("djinn-graph"));
  });

  it("falls back to the community when the node has no file path", () => {
    const sym: SnapshotNode = {
      id: "s",
      kind: "symbol",
      label: "x",
      pagerank: 0,
      symbol_kind: "function",
      community_id: "cluster-7",
    };
    expect(colorForNode(sym)).toBe(colorForGroup("cluster-7"));
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

// ── Community hull derivation (background metadata, not nodes) ──────────────

const communitySnapshot: SnapshotPayload = {
  project_id: "proj-test",
  git_head: "deadbeef",
  generated_at: "2026-06-16T00:00:00Z",
  truncated: false,
  total_nodes: 3,
  total_edges: 2,
  node_cap: 10_000,
  nodes: [
    {
      id: "community:auth",
      kind: "community",
      label: "auth",
      pagerank: 0.5,
      community_id: "auth",
      member_count: 120,
      internal_edge_count: 340,
      workspace_kind: "single",
    },
    {
      id: "community:api",
      kind: "community",
      label: "api",
      pagerank: 0.3,
      community_id: "api",
      member_count: 5000,
      internal_edge_count: 12_000,
      workspace_kind: "mixed",
    },
    {
      id: "community:utils",
      kind: "community",
      label: "utils",
      pagerank: 0.2,
      community_id: "utils",
      member_count: 8,
      internal_edge_count: 3,
    },
  ],
  edges: [
    {
      from: "community:auth",
      to: "community:api",
      kind: "SymbolReference",
      confidence: 0.8,
    },
    {
      from: "community:api",
      to: "community:utils",
      kind: "FileReference",
      confidence: 0.6,
    },
  ],
};

describe("crateForPath / colorForGroup", () => {
  it("extracts the crate name from a cargo workspace path", () => {
    expect(crateForPath("server/crates/djinn-graph/src/lib.rs")).toBe(
      "djinn-graph",
    );
    expect(crateForPath("crates/foo/mod.rs")).toBe("foo");
    // directory node (no trailing file) still resolves the crate
    expect(crateForPath("server/crates/djinn-agent")).toBe("djinn-agent");
  });

  it("uses the first path segment when there is no crates/ marker", () => {
    expect(crateForPath("ui/src/lib/codeGraphAdapter.ts")).toBe("ui");
    expect(crateForPath("server/src/main.rs")).toBe("server");
    expect(crateForPath("README.md")).toBe("README.md");
  });

  it("colorForGroup is deterministic and from the group palette", () => {
    expect(colorForGroup("djinn-graph")).toBe(colorForGroup("djinn-graph"));
    expect(GROUP_COLORS).toContain(colorForGroup("djinn-graph"));
  });

  it("spreads distinct crates across multiple hues (not all one color)", () => {
    const seen = new Set<string>();
    for (const c of [
      "djinn-graph",
      "djinn-agent",
      "djinn-control-plane",
      "djinn-k8s",
      "djinn-provider",
      "djinn-db",
      "ui",
      "server",
    ]) {
      seen.add(colorForGroup(c));
    }
    expect(seen.size).toBeGreaterThanOrEqual(4);
  });
});

describe("computeCrateLegend", () => {
  function snapshotFor(paths: string[]): SnapshotPayload {
    return {
      project_id: "p",
      git_head: "h",
      generated_at: "2026-04-28T00:00:00Z",
      truncated: false,
      total_nodes: paths.length,
      total_edges: 0,
      nodes: paths.map((fp, i) => ({
        id: `symbol:n${i}`,
        kind: "symbol",
        label: `n${i}`,
        symbol_kind: "function",
        pagerank: 0.1,
        x: i,
        y: i,
        file_path: fp,
      })),
      edges: [],
    };
  }

  it("tallies crate groups with hue + count, sorted by count desc then key", () => {
    const graph = buildGraphFromSnapshot(
      snapshotFor([
        "server/crates/djinn-graph/src/a.rs",
        "server/crates/djinn-graph/src/b.rs",
        "server/crates/djinn-graph/src/c.rs",
        "server/crates/djinn-agent/src/a.rs",
        "ui/src/main.ts",
        "ui/src/app.ts",
      ]),
    );
    const legend = computeCrateLegend(graph);
    expect(legend.map((e) => [e.key, e.count])).toEqual([
      ["djinn-graph", 3],
      ["ui", 2],
      ["djinn-agent", 1],
    ]);
    // Hues come from the group palette and match colorForGroup.
    expect(legend[0].color).toBe(colorForGroup("djinn-graph"));
  });

  it("returns an empty legend for a graph with no grouped nodes", () => {
    const graph = buildGraphFromSnapshot(snapshotFor([]));
    expect(computeCrateLegend(graph)).toEqual([]);
  });
});

describe("colorForNode (community)", () => {
  it("colors community nodes by their stable community_id", () => {
    const node: SnapshotNode = {
      id: "community:auth",
      kind: "community",
      label: "auth",
      pagerank: 0,
      community_id: "auth",
    };
    expect(colorForNode(node)).toBe(colorForGroup("auth"));
  });

  it("falls back to hashing the label when community_id is absent", () => {
    const node: SnapshotNode = {
      id: "community:auth",
      kind: "community",
      label: "auth-fallback",
      pagerank: 0,
    };
    expect(colorForNode(node)).toBe(colorForGroup("auth-fallback"));
    expect(GROUP_COLORS).toContain(colorForNode(node));
  });
});

describe("deriveCommunityHulls", () => {
  it("derives one hull per legacy community snapshot entry", () => {
    const hulls = deriveCommunityHulls(communitySnapshot);
    expect(hulls).toHaveLength(3);
    const ids = hulls.map((h) => h.id).sort();
    expect(ids).toEqual(["api", "auth", "utils"]);
  });

  it("hull color matches colorForCommunity so members and hulls align", () => {
    const hulls = deriveCommunityHulls(communitySnapshot);
    for (const hull of hulls) {
      expect(hull.color).toBe(colorForCommunity(hull.id));
      expect(COMMUNITY_COLORS).toContain(hull.color);
    }
  });

  it("folds legacy label and member_count into hull metadata", () => {
    const hulls = deriveCommunityHulls(communitySnapshot);
    const auth = hulls.find((h) => h.id === "auth");
    expect(auth?.label).toBe("auth");
    expect(auth?.memberCount).toBe(120);
    const api = hulls.find((h) => h.id === "api");
    expect(api?.memberCount).toBe(5000);
  });

  it("falls back to member count when no legacy community entry exists", () => {
    const snapshot: SnapshotPayload = {
      ...fixtureSnapshot,
      nodes: fixtureSnapshot.nodes.map((n) =>
        n.kind === "symbol" ? { ...n, community_id: "alpha" } : n,
      ),
    };
    const hulls = deriveCommunityHulls(snapshot);
    const alpha = hulls.find((h) => h.id === "alpha");
    // Two symbols in fixtureSnapshot carry community_id "alpha".
    expect(alpha?.memberCount).toBe(2);
    // No legacy entry → label falls back to the id, but the visible member ids
    // are retained so the canvas can draw a non-clickable background hull.
    expect(alpha?.label).toBe("alpha");
    expect(alpha?.memberIds.sort()).toEqual([
      "symbol:scip-rust . . . User#",
      "symbol:scip-rust . . . main()",
    ]);
  });

  it("colors the hull by its members' dominant crate so it matches the dots", () => {
    // Three members carry community_id "alpha": two in crate "graph",
    // one in crate "core". The hull should take the majority crate's hue,
    // which is exactly what colorForNode assigns those member dots.
    const snapshot: SnapshotPayload = {
      ...fixtureSnapshot,
      nodes: [
        {
          id: "m1",
          kind: "symbol",
          label: "m1",
          pagerank: 0,
          symbol_kind: "function",
          community_id: "alpha",
          workspace: "graph",
        },
        {
          id: "m2",
          kind: "symbol",
          label: "m2",
          pagerank: 0,
          symbol_kind: "function",
          community_id: "alpha",
          workspace: "graph",
        },
        {
          id: "m3",
          kind: "symbol",
          label: "m3",
          pagerank: 0,
          symbol_kind: "function",
          community_id: "alpha",
          workspace: "core",
        },
      ],
      edges: [],
    };
    const hulls = deriveCommunityHulls(snapshot);
    const alpha = hulls.find((h) => h.id === "alpha");
    expect(alpha?.color).toBe(colorForWorkspace("graph"));
  });

  it("is deterministic across calls", () => {
    const a = deriveCommunityHulls(communitySnapshot);
    const b = deriveCommunityHulls(communitySnapshot);
    expect(a).toEqual(b);
  });

  it("hull seed is a stable deterministic hash of the id", () => {
    const hulls = deriveCommunityHulls(communitySnapshot);
    for (const hull of hulls) {
      expect(hull.seed).toBe(hull.seed); // deterministic
      expect(Number.isFinite(hull.seed)).toBe(true);
    }
    // Same id → same seed across calls.
    const again = deriveCommunityHulls(communitySnapshot);
    expect(again[0]?.seed).toBe(hulls[0]?.seed);
  });

  it("returns an empty array when no community nodes or member ids exist", () => {
    expect(deriveCommunityHulls(fixtureSnapshot)).toEqual([]);
  });
});

describe("selectRenderableHulls", () => {
  function hull(id: string, memberCount: number): CommunityHull {
    return {
      id,
      label: id,
      keywords: undefined,
      color: "#22c55e",
      memberCount,
      memberIds: [],
      seed: 1,
    };
  }

  it("drops hulls below the minimum member count", () => {
    const selected = selectRenderableHulls([
      hull("big", 50),
      hull("tiny", MIN_HULL_MEMBERS - 1),
      hull("edge", MIN_HULL_MEMBERS),
    ]);
    expect(selected.map((h) => h.id).sort()).toEqual(["big", "edge"]);
  });

  it("keeps only the largest MAX_HULL_REGIONS, by member count", () => {
    const many = Array.from({ length: MAX_HULL_REGIONS + 10 }, (_, i) =>
      hull(`c${i}`, 10 + i),
    );
    const selected = selectRenderableHulls(many);
    expect(selected).toHaveLength(MAX_HULL_REGIONS);
    // The smallest kept must be larger than the largest dropped.
    const keptMin = Math.min(...selected.map((h) => h.memberCount));
    expect(keptMin).toBe(10 + 10); // c0..c9 (counts 10..19) are dropped
  });

  it("is deterministic for equal member counts (id tiebreak)", () => {
    const a = selectRenderableHulls([hull("b", 8), hull("a", 8)]);
    expect(a.map((h) => h.id)).toEqual(["a", "b"]);
  });
});

describe("buildGraphFromSnapshot (community hulls)", () => {
  it("does not create visible graph nodes for community snapshot entries", () => {
    const graph = buildGraphFromSnapshot(communitySnapshot);
    expect(graph.order).toBe(0);
    expect(graph.hasNode("community:auth")).toBe(false);
    expect(graph.hasNode("community:api")).toBe(false);
    expect(graph.hasNode("community:utils")).toBe(false);
  });

  it("stores derived hulls under the communityHulls graph attribute", () => {
    const graph = buildGraphFromSnapshot(communitySnapshot);
    const hulls = graph.getAttribute(COMMUNITY_HULLS_ATTRIBUTE) as
      | CommunityHull[]
      | undefined;
    expect(hulls).toBeDefined();
    expect(hulls).toHaveLength(3);
    const ids = hulls.map((h) => h.id).sort();
    expect(ids).toEqual(["api", "auth", "utils"]);
  });

  it("drops edges whose endpoints are community nodes (no longer in graph)", () => {
    const graph = buildGraphFromSnapshot(communitySnapshot);
    // All edges touch community node endpoints, so none survive.
    expect(graph.size).toBe(0);
  });

  it("does not set the communityHulls attribute when no communities exist", () => {
    const graph = buildGraphFromSnapshot(fixtureSnapshot);
    expect(graph.getAttribute(COMMUNITY_HULLS_ATTRIBUTE)).toBeUndefined();
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

describe("containment edge kinds", () => {
  it("CONTAINMENT_EDGE_KINDS includes ContainsDefinition, DeclaredInFile, and MemberOf", () => {
    expect(CONTAINMENT_EDGE_KINDS.has("ContainsDefinition")).toBe(true);
    expect(CONTAINMENT_EDGE_KINDS.has("DeclaredInFile")).toBe(true);
    expect(CONTAINMENT_EDGE_KINDS.has("MemberOf")).toBe(true);
  });

  it("isContainmentEdgeKind returns true only for containment kinds", () => {
    expect(isContainmentEdgeKind("ContainsDefinition")).toBe(true);
    expect(isContainmentEdgeKind("DeclaredInFile")).toBe(true);
    expect(isContainmentEdgeKind("MemberOf")).toBe(true);
    expect(isContainmentEdgeKind("SymbolReference")).toBe(false);
    expect(isContainmentEdgeKind("Reads")).toBe(false);
    expect(isContainmentEdgeKind("Extends")).toBe(false);
  });
});

describe("isDoubleClick", () => {
  it("returns false for the first click (no previous)", () => {
    expect(isDoubleClick(null, "node-a", 1000)).toBe(false);
  });

  it("returns true for a second click on the same node within the interval", () => {
    const prev = { nodeId: "node-a", at: 1000 };
    expect(isDoubleClick(prev, "node-a", 1100)).toBe(true);
  });

  it("returns false when the second click hits a different node", () => {
    const prev = { nodeId: "node-a", at: 1000 };
    expect(isDoubleClick(prev, "node-b", 1100)).toBe(false);
  });

  it("returns false when the interval exceeds the threshold", () => {
    const prev = { nodeId: "node-a", at: 1000 };
    expect(
      isDoubleClick(prev, "node-a", 1000 + DOUBLE_CLICK_INTERVAL_MS + 1),
    ).toBe(false);
  });

  it("returns true at exactly the interval boundary (inclusive)", () => {
    const prev = { nodeId: "node-a", at: 1000 };
    expect(isDoubleClick(prev, "node-a", 1000 + DOUBLE_CLICK_INTERVAL_MS)).toBe(
      true,
    );
  });

  it("respects a custom interval override", () => {
    const prev = { nodeId: "node-a", at: 1000 };
    expect(isDoubleClick(prev, "node-a", 1050, 100)).toBe(true);
    expect(isDoubleClick(prev, "node-a", 1200, 100)).toBe(false);
  });
});

// ── LOD tier helpers ──────────────────────────────────────────────────────

describe("lodTierForZoom", () => {
  it("returns 'far' when camera ratio >= LOD_FAR_RATIO", () => {
    expect(lodTierForZoom(LOD_FAR_RATIO)).toBe("far");
    expect(lodTierForZoom(3.0)).toBe("far");
    expect(lodTierForZoom(100)).toBe("far");
  });

  it("returns 'mid' when camera ratio is between MID and FAR thresholds", () => {
    expect(lodTierForZoom(LOD_MID_RATIO)).toBe("mid");
    expect(lodTierForZoom(1.0)).toBe("mid");
    expect(lodTierForZoom(LOD_FAR_RATIO - 0.01)).toBe("mid");
  });

  it("returns 'close' when camera ratio < LOD_MID_RATIO", () => {
    expect(lodTierForZoom(LOD_MID_RATIO - 0.01)).toBe("close");
    expect(lodTierForZoom(0.1)).toBe("close");
    expect(lodTierForZoom(0.001)).toBe("close");
  });

  it("returns 'close' for non-finite camera ratios", () => {
    expect(lodTierForZoom(Infinity)).toBe("close");
    expect(lodTierForZoom(NaN)).toBe("close");
    expect(lodTierForZoom(-Infinity)).toBe("close");
  });

  it("treats ratio 0 as close (zoomed in past natural extent)", () => {
    expect(lodTierForZoom(0)).toBe("close");
  });
});

describe("isSymbolVisibleAtMidTier", () => {
  it("returns true for top-level / structural symbol kinds", () => {
    for (const kind of [
      "class",
      "struct",
      "interface",
      "trait",
      "enum",
      "function",
      "method",
      "constructor",
      "impl",
      "type",
    ]) {
      expect(isSymbolVisibleAtMidTier(kind)).toBe(true);
    }
  });

  it("returns false for low-priority symbol kinds", () => {
    for (const kind of [
      "variable",
      "const",
      "static",
      "property",
      "field",
      "import",
      "other",
    ]) {
      expect(isSymbolVisibleAtMidTier(kind)).toBe(false);
    }
  });

  it("returns true when symbol kind is undefined (treat as structural)", () => {
    expect(isSymbolVisibleAtMidTier(undefined)).toBe(true);
  });
});

// ── Viewport culling ─────────────────────────────────────────────────────

describe("viewportBoundsEqual", () => {
  it("treats distinct objects with equal fields as equal (no spurious refresh)", () => {
    // This is the regression guard: getViewportBounds() returns a fresh
    // object each call, so the afterRender handler must compare by VALUE.
    // Identity comparison here would be `false` and spin a render loop.
    const a: ViewportBounds = { minX: 0, minY: 0, maxX: 100, maxY: 100 };
    const b: ViewportBounds = { minX: 0, minY: 0, maxX: 100, maxY: 100 };
    expect(a).not.toBe(b); // distinct references
    expect(viewportBoundsEqual(a, b)).toBe(true);
  });

  it("detects a real viewport change", () => {
    const a: ViewportBounds = { minX: 0, minY: 0, maxX: 100, maxY: 100 };
    expect(viewportBoundsEqual(a, { ...a, maxX: 101 })).toBe(false);
    expect(viewportBoundsEqual(a, { ...a, minY: -1 })).toBe(false);
  });

  it("treats two nulls (culling inactive) as equal and null/object as changed", () => {
    expect(viewportBoundsEqual(null, null)).toBe(true);
    const a: ViewportBounds = { minX: 0, minY: 0, maxX: 1, maxY: 1 };
    expect(viewportBoundsEqual(a, null)).toBe(false);
    expect(viewportBoundsEqual(null, a)).toBe(false);
  });
});

describe("isInViewport", () => {
  const bounds: ViewportBounds = { minX: 0, minY: 0, maxX: 100, maxY: 100 };

  it("returns true for a node inside the bounds", () => {
    expect(isInViewport(50, 50, bounds)).toBe(true);
  });

  it("returns true for a node on the boundary", () => {
    expect(isInViewport(0, 0, bounds)).toBe(true);
    expect(isInViewport(100, 100, bounds)).toBe(true);
  });

  it("returns true for a node just outside bounds but within margin", () => {
    // VIEWPORT_CULLING_MARGIN is the default margin
    const m = VIEWPORT_CULLING_MARGIN;
    expect(isInViewport(-m + 1, 50, bounds)).toBe(true);
    expect(isInViewport(100 + m - 1, 50, bounds)).toBe(true);
  });

  it("returns false for a node outside bounds + margin", () => {
    const m = VIEWPORT_CULLING_MARGIN;
    expect(isInViewport(-m - 10, 50, bounds)).toBe(false);
    expect(isInViewport(100 + m + 10, 50, bounds)).toBe(false);
    expect(isInViewport(50, -m - 10, bounds)).toBe(false);
    expect(isInViewport(50, 100 + m + 10, bounds)).toBe(false);
  });

  it("works with negative coordinate bounds", () => {
    const negBounds: ViewportBounds = {
      minX: -200,
      minY: -200,
      maxX: -100,
      maxY: -100,
    };
    expect(isInViewport(-150, -150, negBounds)).toBe(true);
    // (0, 0) is outside bounds but within VIEWPORT_CULLING_MARGIN;
    // (500, 500) is far enough outside to be culled.
    expect(isInViewport(500, 500, negBounds)).toBe(false);
  });
});

describe("VIEWPORT_CULLING_THRESHOLD", () => {
  it("is 8000 (matches the large-graph target)", () => {
    expect(VIEWPORT_CULLING_THRESHOLD).toBe(8_000);
  });
});
