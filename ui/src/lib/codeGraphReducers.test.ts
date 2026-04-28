import { describe, expect, it } from "vitest";

import {
  EMPTY_HIGHLIGHT_VIEW,
  bfsReachable,
  edgeReducer,
  isViewEmpty,
  nodeReducer,
  oneHopNeighborhood,
  pickHighlightMode,
  type HighlightView,
  type MinimalGraph,
} from "./codeGraphReducers";

function viewWith(overrides: Partial<HighlightView>): HighlightView {
  return { ...EMPTY_HIGHLIGHT_VIEW, ...overrides };
}

/** Tiny adjacency-list graph for the BFS / 1-hop tests. */
function makeGraph(
  edges: Array<[string, string]>,
  nodes?: string[],
): MinimalGraph {
  const adj = new Map<string, Set<string>>();
  const ensureNode = (id: string) => {
    if (!adj.has(id)) adj.set(id, new Set());
  };
  for (const id of nodes ?? []) ensureNode(id);
  for (const [a, b] of edges) {
    ensureNode(a);
    ensureNode(b);
    adj.get(a)!.add(b);
    adj.get(b)!.add(a); // undirected for the highlight reducer
  }
  return {
    hasNode: (id) => adj.has(id),
    neighbors: (id) => Array.from(adj.get(id) ?? []),
  };
}

describe("isViewEmpty", () => {
  it("is true for the default view", () => {
    expect(isViewEmpty(EMPTY_HIGHLIGHT_VIEW)).toBe(true);
  });

  it("is false once selection is set", () => {
    expect(isViewEmpty(viewWith({ selectionId: "a" }))).toBe(false);
  });

  it("is false when any highlight set is non-empty", () => {
    expect(isViewEmpty(viewWith({ citationIds: new Set(["x"]) }))).toBe(false);
    expect(
      isViewEmpty(viewWith({ toolHighlightIds: new Set(["y"]) })),
    ).toBe(false);
    expect(
      isViewEmpty(viewWith({ blastRadiusFrontier: new Set(["z"]) })),
    ).toBe(false);
  });

  it("is false when hover is set", () => {
    expect(isViewEmpty(viewWith({ hoverId: "h" }))).toBe(false);
  });
});

describe("pickHighlightMode", () => {
  it("returns 'none' on the empty view", () => {
    expect(pickHighlightMode("a", EMPTY_HIGHLIGHT_VIEW)).toBe("none");
  });

  it("focuses the selection node", () => {
    const v = viewWith({ selectionId: "a" });
    expect(pickHighlightMode("a", v)).toBe("focus");
  });

  it("highlights neighbors of the selection", () => {
    const v = viewWith({
      selectionId: "a",
      selectionNeighbors: new Set(["a", "b"]),
    });
    expect(pickHighlightMode("b", v)).toBe("neighbor");
  });

  it("dims unrelated nodes when a selection exists", () => {
    const v = viewWith({
      selectionId: "a",
      selectionNeighbors: new Set(["a"]),
    });
    expect(pickHighlightMode("z", v)).toBe("dim");
  });

  it("blast radius beats tool highlight", () => {
    const v = viewWith({
      blastRadiusFrontier: new Set(["a"]),
      toolHighlightIds: new Set(["a"]),
    });
    expect(pickHighlightMode("a", v)).toBe("blast");
  });

  it("tool highlight beats citation", () => {
    const v = viewWith({
      toolHighlightIds: new Set(["a"]),
      citationIds: new Set(["a"]),
    });
    expect(pickHighlightMode("a", v)).toBe("tool");
  });

  it("citation beats neighbor", () => {
    const v = viewWith({
      citationIds: new Set(["b"]),
      selectionId: "a",
      selectionNeighbors: new Set(["a", "b"]),
    });
    expect(pickHighlightMode("b", v)).toBe("citation");
  });

  it("hover surfaces only when nothing else applies", () => {
    const v = viewWith({ hoverId: "h" });
    expect(pickHighlightMode("h", v)).toBe("hover");
    // Selection wins over hover
    expect(
      pickHighlightMode(
        "h",
        viewWith({ hoverId: "h", selectionId: "h" }),
      ),
    ).toBe("focus");
  });
});

describe("nodeReducer", () => {
  it("passes attrs through unchanged when view is empty", () => {
    const attrs = { color: "blue", size: 5, label: "Foo" };
    const out = nodeReducer("a", attrs, EMPTY_HIGHLIGHT_VIEW);
    expect(out).toBe(attrs);
  });

  it("hides nodes outside the depth frontier", () => {
    const v = viewWith({
      selectionId: "a",
      depthReachable: new Set(["a"]),
    });
    const out = nodeReducer("z", { color: "blue", size: 5 }, v);
    expect(out.hidden).toBe(true);
  });

  it("paints the focal node orange and grows it", () => {
    const v = viewWith({
      selectionId: "a",
      selectionNeighbors: new Set(["a"]),
    });
    const out = nodeReducer("a", { color: "blue", size: 4 }, v);
    expect(out.color).toBe("#f97316");
    expect(out.size).toBe(4 * 1.6);
    expect(out.highlighted).toBe(true);
  });

  it("dims a non-neighbor when selection is active", () => {
    const v = viewWith({
      selectionId: "a",
      selectionNeighbors: new Set(["a"]),
    });
    const out = nodeReducer("z", { color: "blue", size: 4, label: "Z" }, v);
    expect(out.color).toMatch(/rgba/);
    expect(out.label).toBeUndefined();
    expect(out.highlighted).toBe(false);
  });

  it("renders citation nodes in sky-blue", () => {
    const v = viewWith({ citationIds: new Set(["c"]) });
    const out = nodeReducer("c", { color: "blue", size: 4 }, v);
    expect(out.color).toBe("#38bdf8");
  });

  it("renders tool-highlight nodes in violet", () => {
    const v = viewWith({ toolHighlightIds: new Set(["t"]) });
    const out = nodeReducer("t", { color: "blue", size: 4 }, v);
    expect(out.color).toBe("#a78bfa");
  });

  it("blast-radius pulse interpolates color across the phase cycle", () => {
    const lo = viewWith({
      blastRadiusFrontier: new Set(["b"]),
      pulsePhase: 0,
    });
    const hi = viewWith({
      blastRadiusFrontier: new Set(["b"]),
      pulsePhase: 0.5,
    });
    const outLo = nodeReducer("b", { color: "blue", size: 4 }, lo);
    const outHi = nodeReducer("b", { color: "blue", size: 4 }, hi);
    expect(outLo.color).not.toBe(outHi.color);
  });
});

describe("edgeReducer", () => {
  it("passes through when view is empty and no kind filter applies", () => {
    const attrs = { color: "gray", size: 1, kind: "Reads" };
    const out = edgeReducer("a", "b", attrs, EMPTY_HIGHLIGHT_VIEW);
    expect(out).toBe(attrs);
  });

  it("hides edges of disabled kinds", () => {
    const v = viewWith({ edgeKindFilters: { Reads: false } });
    const out = edgeReducer("a", "b", { kind: "Reads" }, v);
    expect(out.hidden).toBe(true);
  });

  it("treats unknown edge kinds as visible (no filter entry)", () => {
    const v = viewWith({ edgeKindFilters: {} });
    const out = edgeReducer("a", "b", { kind: "MysteryKind" }, v);
    expect(out.hidden).toBeUndefined();
  });

  it("hides edges that cross the depth frontier", () => {
    const v = viewWith({
      selectionId: "a",
      depthReachable: new Set(["a", "b"]),
    });
    const out = edgeReducer("a", "z", { kind: "Reads" }, v);
    expect(out.hidden).toBe(true);
  });

  it("highlights edges incident on the selection 1-hop frontier", () => {
    const v = viewWith({
      selectionId: "a",
      selectionNeighbors: new Set(["a", "b"]),
    });
    const out = edgeReducer("a", "b", { kind: "Reads", size: 1 }, v);
    expect(out.color).toMatch(/orange|rgba\(251/);
  });

  it("dims unrelated edges when a selection is active", () => {
    const v = viewWith({
      selectionId: "a",
      selectionNeighbors: new Set(["a"]),
    });
    const out = edgeReducer("y", "z", { kind: "Reads", size: 1 }, v);
    expect(out.color).toMatch(/rgba\(100/);
  });
});

describe("oneHopNeighborhood", () => {
  it("returns empty set for unknown node", () => {
    const g = makeGraph([["a", "b"]]);
    expect(oneHopNeighborhood(g, "missing").size).toBe(0);
  });

  it("includes the seed itself", () => {
    const g = makeGraph([["a", "b"]]);
    const ns = oneHopNeighborhood(g, "a");
    expect(ns.has("a")).toBe(true);
  });

  it("walks undirected neighbors", () => {
    const g = makeGraph([
      ["a", "b"],
      ["c", "a"],
    ]);
    const ns = oneHopNeighborhood(g, "a");
    expect(ns.has("b")).toBe(true);
    expect(ns.has("c")).toBe(true);
  });
});

describe("bfsReachable", () => {
  const g = makeGraph([
    ["a", "b"],
    ["b", "c"],
    ["c", "d"],
    ["d", "e"],
  ]);

  it("returns just the seed at depth 0", () => {
    const r = bfsReachable(g, "a", 0);
    expect(Array.from(r).sort()).toEqual(["a"]);
  });

  it("walks 1 hop at depth 1", () => {
    const r = bfsReachable(g, "a", 1);
    expect(Array.from(r).sort()).toEqual(["a", "b"]);
  });

  it("walks 3 hops at depth 3", () => {
    const r = bfsReachable(g, "a", 3);
    expect(Array.from(r).sort()).toEqual(["a", "b", "c", "d"]);
  });

  it("walks the entire connected component when depth >> diameter", () => {
    const r = bfsReachable(g, "a", 100);
    expect(Array.from(r).sort()).toEqual(["a", "b", "c", "d", "e"]);
  });

  it("returns empty set for unknown seed", () => {
    expect(bfsReachable(g, "missing", 5).size).toBe(0);
  });
});
